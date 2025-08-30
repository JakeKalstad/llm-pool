use std::{
    str::{self, FromStr},
    time::SystemTime,
};

use crate::{opensearch_api, s3, LLMServer, State};
use async_std::stream::StreamExt;
use bb8::PooledConnection;
use bb8_redis::RedisConnectionManager;
use chrono::{DateTime, Utc};
use opensearch::OpenSearch;
use redis::{aio::MultiplexedConnection, AsyncCommands, Client};
use reqwest::{header, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tide::{http::mime, Request};
use tokio::task;

use rand::rng;
use rand::seq::IndexedRandom;
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
pub struct RequestBody {
    pub model: String,
    pub prompt: String,
    pub stream: Option<bool>,
    pub contextualize: Option<bool>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct EmbedRequestBody {
    pub model: String,
    pub input: String,
    pub stream: Option<bool>,
}

#[derive(Deserialize, Serialize)]
pub struct ApiResponse {
    pub key: Option<String>,
    pub model: String,
    pub created_at: String,
    pub response: String,
    pub done: bool,

    pub ms_taken: Option<u128>,
    pub server_url: Option<String>,
    pub original_api_request: Option<RequestBody>,
}

pub async fn pop_queue(
    pool: std::sync::Arc<bb8::Pool<RedisConnectionManager>>,
    possible_servers: &Vec<LLMServer>,
    redis_expiration: u64,
    s3_url: String,
    s3_user: String,
    s3_pass: String,
    s3_expiration: i32,
    s3_bucket: String,
    opensearch_servers: &Vec<crate::OSServer>,
    opensearch_user: String,
    opensearch_password: String,
    opensearch_index: String,
    opensearch_clients: std::sync::Arc<Vec<OpenSearch>>,
    embedding_model: String,
) {
    let now = SystemTime::now();
    let datetime: DateTime<Utc> = now.into(); // Convert to UTC datetime
    let timestamp_str = datetime.format("%Y-%m-%d %H:%M:%S").to_string();

    let mut redis = pool.get().await.expect("get redis conn");
    let pattern = "queue-*".to_string();
    let keys = scan_keys(&mut redis.clone(), pattern)
        .await
        .expect("scan available queue keys");

    for k in keys {
        let queued_request_uuid: String = redis.get(&k).await.unwrap_or_default();
        let queued_response_json: String =
            redis.get(&queued_request_uuid).await.unwrap_or_default();

        let queued_response: ApiResponse = serde_json::from_str(&queued_response_json)
            .expect(&format!("unmarshal api request from queue {k}"));
        let api_request = queued_response
            .original_api_request
            .expect("queued responses should have origin requests available");
        let prompt_hash = hash_string(&api_request.prompt);
        let request_prompt_key = prompt_cache_string(&prompt_hash, &api_request.model);
        let prompt: bool = redis.exists(&request_prompt_key).await.unwrap_or_default();
        if prompt {
            // delete the key it's already in flight
            let _s: String = redis
                .del(&k)
                .await
                .expect("removed in flight-already queued key");
            continue;
        };

        // send the request to llm
        let response_id = Uuid::from_str(queued_response.key.unwrap_or_default().as_str())
            .expect("queued response must have a key");

        let server_option = find_available_server(&mut possible_servers.clone(), &mut redis).await;
        match server_option {
            Some(server) => {
                llm_request(
                    pool.clone(),
                    redis_expiration,
                    s3_url.clone(),
                    s3_user.clone(),
                    s3_pass.clone(),
                    s3_expiration.clone(),
                    s3_bucket.clone(),
                    timestamp_str.clone(),
                    server.clone(),
                    api_request.clone(),
                    response_id.clone(),
                    opensearch_servers.clone(),
                    opensearch_user.clone(),
                    opensearch_password.clone(),
                    opensearch_index.clone(),
                    opensearch_clients.clone(),
                    embedding_model.clone(),
                )
                .await;

                let _rset_res: String = redis
                    .set_ex(
                        &request_prompt_key,
                        &response_id.to_string(),
                        redis_expiration,
                    )
                    .await
                    .unwrap();

                let _s: String = redis.del(&k).await.expect("removed");
            }
            None => continue,
        }
    }
}

pub(crate) async fn scan_keys(
    con: &mut impl redis::aio::ConnectionLike,
    pattern: String,
) -> redis::RedisResult<Vec<String>> {
    let mut cursor = 0;
    let mut keys = Vec::new();

    loop {
        let (new_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern.clone())
            .arg("COUNT")
            .arg(100)
            .query_async(con)
            .await?;

        keys.extend(batch);
        cursor = new_cursor;

        if cursor == 0 {
            break;
        }
    }

    Ok(keys)
}

pub async fn find_available_server(
    possible_servers: &mut Vec<LLMServer>,
    con: &mut PooledConnection<'_, RedisConnectionManager>,
) -> Option<LLMServer> {
    while let Some(server) = select_best_server(&possible_servers.clone()) {
        let current_in_flight: i64 = con.get(&server.url).await.unwrap_or_default();

        if current_in_flight < server.max_concurrent {
            return Some(server);
        }

        possible_servers.retain(|s| s.url != server.url);
    }

    None
}

#[derive(Clone, Deserialize, Serialize)]
pub struct GenerateResponse {
    pub response_id: String,
    pub queued: bool,
}
pub async fn request(mut req: Request<State>) -> tide::Result {
    let api_request: RequestBody = req.body_json().await.expect("unmarshal json");

    let state = req.state();
    let mut redis = state.pool.get().await?;
    let possible_servers = state.possible_servers.clone();

    let prompt_hash = hash_string(&api_request.prompt);
    let request_prompt_key = prompt_cache_string(&prompt_hash, &api_request.model);
    let prompt: bool = redis.exists(&request_prompt_key).await.unwrap_or_default();
    if prompt {
        let response_id: String = redis
            .get(&request_prompt_key)
            .await
            .expect("get response cache key");

        let response = serde_json::to_string(&GenerateResponse {
            response_id: response_id.to_string(),
            queued: false,
        })
        .expect("get response");
        // let response: String = redis.get(&response_id).await.expect("get cached response");
        return Ok(tide::Response::builder(tide::StatusCode::Ok)
            .content_type(mime::JSON)
            .body(response)
            .build());
    };

    // send the request to llm
    let response_id = uuid::Uuid::new_v4();
    let now = SystemTime::now();
    let datetime: DateTime<Utc> = now.into(); // Convert to UTC datetime
    let timestamp_str = datetime.format("%Y-%m-%d %H:%M:%S").to_string();
    let redis_expiration = state.redis_expiration;

    let server_option = find_available_server(&mut possible_servers.clone(), &mut redis).await;
    match server_option {
        Some(server) => {
            task::spawn(llm_request(
                state.pool.clone(),
                redis_expiration.clone(),
                state.s3_url.clone(),
                state.s3_user.clone(),
                state.s3_pass.clone(),
                state.s3_expiration,
                state.s3_bucket.clone(),
                timestamp_str.clone(),
                server.clone(),
                api_request.clone(),
                response_id.clone(),
                state.opensearch_servers.clone(),
                state.opensearch_user.clone(),
                state.opensearch_password.clone(),
                state.opensearch_index.clone(),
                state.opensearch_clients.clone(),
                state.embedding_model.clone(),
            ));

            let res_obj = ApiResponse {
                key: Some(response_id.to_string()),
                response: String::new(),
                created_at: timestamp_str.clone(),
                done: false,
                model: api_request.model.clone(),
                ms_taken: None,
                server_url: Some(server.url),
                original_api_request: Some(api_request.clone()),
            };

            let data = json!(res_obj).to_string();
            let _rset_res: String = redis
                .set_ex(response_id.to_string(), data.as_str(), redis_expiration)
                .await
                .unwrap();
            let _rset_res: String = redis
                .set_ex(
                    &request_prompt_key,
                    &response_id.to_string(),
                    redis_expiration,
                )
                .await
                .unwrap();
            let response = serde_json::to_string(&GenerateResponse {
                response_id: response_id.to_string(),
                queued: true,
            })
            .expect("get response");
            Ok(tide::Response::builder(tide::StatusCode::Ok)
                .content_type(mime::JSON)
                .body(response)
                .build())
        }
        None => {
            let res_obj = ApiResponse {
                key: Some(response_id.to_string()),
                response: String::new(),
                created_at: timestamp_str.clone(),
                done: false,
                model: api_request.model.clone(),
                ms_taken: None,
                server_url: None,
                original_api_request: Some(api_request.clone()),
            };

            let request_prompt_queue_key =
                queue_cache_string(&prompt_hash, &api_request.clone().model.clone());

            let mut queued_response_id: String = redis
                .get(&request_prompt_queue_key)
                .await
                .unwrap_or_default();

            if queued_response_id.len() == 0 {
                let _setting_prompt_to_response_id: String = redis
                    .set_ex(
                        &request_prompt_queue_key,
                        response_id.to_string().as_str(),
                        redis_expiration,
                    )
                    .await
                    .expect("set the queue key");

                let data = json!(res_obj).to_string();
                let _setting_prompt_to_response_id: String = redis
                    .set_ex(&response_id.to_string(), data.as_str(), redis_expiration)
                    .await
                    .expect("set the queue key");
                queued_response_id = response_id.to_string();
            };

            let response = serde_json::to_string(&GenerateResponse {
                response_id: queued_response_id.to_string(),
                queued: true,
            })
            .expect("get response");

            return Ok(tide::Response::builder(tide::StatusCode::TooManyRequests)
                .content_type(mime::JSON)
                .body(response)
                .build());
        }
    }
}

pub async fn get(req: Request<State>) -> tide::Result {
    let id: String = req.param("id")?.to_string();
    let mut redis = req.state().pool.get().await?;
    let r: String = redis.get(id).await.unwrap_or_default();

    Ok(tide::Response::builder(tide::StatusCode::Ok)
        .content_type(mime::JSON)
        .body(r)
        .build())
}

async fn llm_request(
    pool: std::sync::Arc<bb8::Pool<RedisConnectionManager>>,
    redis_expiration: u64,
    s3_url: String,
    s3_user: String,
    s3_pass: String,
    expiration_days: i32,
    s3_bucket: String,
    timestamp_str: String,
    server: LLMServer,
    request_body: RequestBody,
    response_id: uuid::Uuid,

    opensearch_urls: Vec<crate::OSServer>,
    opensearch_user: String,
    opensearch_password: String,
    opensearch_index: String,
    opensearch_clients: std::sync::Arc<Vec<OpenSearch>>,
    embedding_model: String,
) {
    let mut conn = pool.get().await.expect("get redis conn");
    let url = Url::from_str(&format!("{}generate", server.url)).expect("URL");
    let _reset_res: String = conn.incr(&server.url, 1).await.unwrap();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap();
    let now: SystemTime = SystemTime::now();
    let mut res = ApiResponse {
        created_at: String::new(),
        done: false,
        key: None,
        model: String::new(),
        response: String::new(),
        ms_taken: None,
        original_api_request: None,
        server_url: None,
    };
    let mut final_prompt = String::default();
    if request_body.contextualize.unwrap_or_default() {
        let prompt_embedding = embedding_request(
            pool.clone(),
            redis_expiration.clone(),
            timestamp_str.clone(),
            server.clone(),
            EmbedRequestBody {
                model: embedding_model.clone(),
                input: request_body.prompt.clone(),
                stream: Some(false),
            },
            response_id.clone(),
        )
        .await;
        let mut context = String::default();
        for client in opensearch_clients.clone().iter() {
            let search_results = opensearch_api::search_similar(
                &client,
                opensearch_index.clone(),
                prompt_embedding.embeddings.clone(),
            )
            .await
            .expect("search");
            for hit in search_results.hits.hits.iter().clone() {
                res = serde_json::from_str(&hit._source.text.clone()).expect("json valid");

                context = format!(
                    "{} \n\n {} \n {}",
                    context,
                    res.original_api_request.expect("api request exists").prompt,
                    res.response
                );
            }
        }
        final_prompt = format!(
            "For additional context use our previous conversation: {} \n::: prompt :::\n {}\n",
            context,
            request_body.prompt.clone()
        )
    } else {
        final_prompt = request_body.prompt.clone()
    };

    let res: Result<reqwest::Response, reqwest::Error> = client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .timeout(std::time::Duration::from_secs(300))
        .json(&RequestBody {
            contextualize: request_body.contextualize,
            model: request_body.model.clone(),
            prompt: final_prompt.clone(),
            stream: request_body.stream,
        })
        .send()
        .await;
    let res = match res {
        Ok(r) => r,
        Err(e) => {
            println!("{e}");
            return;
        }
    };
    if !res.status().is_success() {
        println!("{:?}", res);
        let _data_cache_res: String = conn
            .del(response_id.to_string())
            .await
            .expect("del val");
        let _reset_res: String = conn.decr(&server.url, 1).await.unwrap();
        return;
    }

    let total_response: Result<String, String> = process_stream(res).await;
    let mut ms_taken: Option<u128> = None;
    match now.elapsed() {
        Ok(elapsed) => ms_taken = Some(elapsed.as_millis()),
        Err(e) => {
            println!("Error: {:?}", e);
        }
    };
    let response: String = total_response.unwrap_or_default();

    let res_obj = ApiResponse {
        key: Some(response_id.to_string()),
        response: response.clone(),
        created_at: timestamp_str.clone(),
        done: true,
        model: request_body.model.clone(),
        ms_taken,
        server_url: Some(server.url.clone()),
        original_api_request: Some(request_body.clone()),
    };
    let data = json!(res_obj).to_string();
    let file_data = data.as_bytes().to_vec();
    let file_name = format!("{response_id}.json").clone();
    let _ = s3::write_file_fs(
        s3_url.clone(),
        s3_user.clone(),
        s3_pass.clone(),
        expiration_days,
        s3_bucket.clone(),
        file_name.clone(),
        file_data.clone(),
    )
    .await;
    if response.len() == 0 {
        let _data_cache_res: String = conn
            .del(response_id.to_string())
            .await
            .expect("del val");
        let _reset_res: String = conn.decr(&server.url, 1).await.unwrap();
        return;
    }
    let embed_request = EmbedRequestBody {
        model: embedding_model.clone(),
        input: data.clone(),
        stream: Some(false),
    };
    let embedding = embedding_request(
        pool.clone(),
        redis_expiration,
        timestamp_str.clone(),
        server.clone(),
        embed_request.clone(),
        response_id.clone(),
    )
    .await;
    for os_client in opensearch_clients.clone().iter() {
        opensearch_api::insert_document(
            &os_client,
            opensearch_index.clone(),
            response_id.to_string(),
            data.clone(),
            embedding.embeddings.clone(),
            Some(file_name.clone()),
            Some("test".to_string()),
            Some("api".to_string()),
            Some(request_body.model.clone()),
            Some(timestamp_str.clone()),
        )
        .await
        .expect("Inserted into opensearch");
    }
    let embedding_data = json!(embedding);
    let embedding_data = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        embedding_data.to_string(),
    );

    let embed_hash = hash_string(&embed_request.input);
    let request_embed_key = embed_cache_string(&embed_hash, &embed_request.model);

    let _embed_cache_res: String = conn
        .set_ex(request_embed_key, embedding_data.as_str(), redis_expiration)
        .await
        .expect("set val");

    let _data_cache_res: String = conn
        .set_ex(response_id.to_string(), data.as_str(), redis_expiration)
        .await
        .expect("set val");
    let _reset_res: String = conn.decr(&server.url, 1).await.unwrap();
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EmbeddingResponse {
    pub model: String,
    pub embeddings: Vec<Vec<f64>>,
}

pub async fn embedding_request(
    pool: std::sync::Arc<bb8::Pool<RedisConnectionManager>>,
    redis_expiration: u64,
    timestamp_str: String,
    server: LLMServer,
    request_body: EmbedRequestBody,
    response_id: uuid::Uuid,
) -> EmbeddingResponse {
    let mut conn = pool.get().await.expect("get redis conn");
    let url = Url::from_str(&format!("{}embed", server.url)).expect("URL");

    let embed_hash = hash_string(&request_body.input);
    let request_embed_key = embed_cache_string(&embed_hash, &request_body.model);
    let embed_cached: bool = conn.exists(&request_embed_key).await.unwrap_or_default();

    if embed_cached {
        let embed_response_json: String = conn
            .get(&request_embed_key)
            .await
            .expect("get response cache key");
        let embed_response_json = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            embed_response_json,
        );
        let response: EmbeddingResponse =
            serde_json::from_str(&embed_response_json).expect("valid json");

        return response;
    };

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap();

    let res = client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .json(&request_body)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .expect("Failed to send request");
    let response: EmbeddingResponse = res.json().await.expect("works");
    let data = json!(response);
    let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data.to_string());
    let _data_cache_res: String = conn
        .set_ex(response_id.to_string(), data.as_str(), redis_expiration)
        .await
        .expect("set val");
    response
}

pub fn select_best_server(servers: &Vec<LLMServer>) -> Option<LLMServer> {
    if servers.is_empty() {
        return None;
    }

    let max_weight = servers.iter().map(|s| s.weight).max().unwrap(); // Get highest weight
    let candidates: Vec<&LLMServer> = servers.iter().filter(|s| s.weight == max_weight).collect();

    let mut rng = rng();
    let c = candidates.choose(&mut rng).copied();
    Some(c.unwrap().clone())
}
async fn process_stream(res: reqwest::Response) -> Result<String, String> {
    let mut buffer = String::new();
    let mut total_response = String::new();
    let mut stream = res.bytes_stream();

    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => {
                let chunk_str = std::str::from_utf8(&chunk).unwrap_or("");
                buffer.push_str(chunk_str);

                // Try to parse complete JSON objects
                while let Some(end) = buffer.find("}") {
                    let json_chunk = &buffer[..=end];
                    match serde_json::from_str::<ApiResponse>(json_chunk) {
                        Ok(api_response) => {
                            total_response.push_str(&api_response.response);
                        }
                        Err(_) => {
                            // Not a complete JSON yet; break and wait for more
                            break;
                        }
                    }
                    buffer.drain(..=end);
                }
            }
            Err(e) => return Err(format!("Stream error: {:?}", e)),
        }
    }

    // Return any leftover buffer if valid JSON
    if !buffer.is_empty() {
        if let Ok(api_response) = serde_json::from_str::<ApiResponse>(&buffer) {
            total_response.push_str(&api_response.response);
        } else {
            // Fallback: include raw text
            total_response.push_str(&buffer);
        }
    }

    Ok(total_response)
}

fn hash_string(input: &str) -> String {
    format!("{:x}", twox_hash::XxHash3_128::oneshot(input.as_bytes()))
}

fn embed_cache_string(embedt_hash: &str, model: &str) -> String {
    format!("embed-{}:{}", embedt_hash, model)
}

fn prompt_cache_string(prompt_hash: &str, model: &str) -> String {
    format!("prompt-{}:{}", prompt_hash, model)
}

fn queue_cache_string(prompt_hash: &str, model: &str) -> String {
    format!("queue-{}:{}", prompt_hash, model)
}
