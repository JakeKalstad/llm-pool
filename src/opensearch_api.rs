use std::time::SystemTime;

use chrono::{DateTime, Utc};
use opensearch::{
    http::transport::{SingleNodeConnectionPool, TransportBuilder},
    indices::{IndicesCreateParts, IndicesExistsParts},
    OpenSearch, SearchParts,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::json;

use tide::{http::mime, Body, Request};
use uuid::Uuid;

use crate::api::{embedding_request, find_available_server, EmbedRequestBody};

pub(crate) fn new(
    url: Url,
    username: String,
    password: String,
) -> Result<OpenSearch, opensearch::http::transport::BuildError> {
    let conn_pool = SingleNodeConnectionPool::new(url);

    let transport = TransportBuilder::new(conn_pool)
        .auth(opensearch::auth::Credentials::Basic(username, password))
        .cert_validation(opensearch::cert::CertificateValidation::None)
        .build()?;

    Ok(OpenSearch::new(transport))
}

#[derive(Clone, Deserialize, Serialize)]
pub struct IndexRequestBody {
    pub content: String,
}

pub async fn index(mut req: Request<crate::State>) -> tide::Result {
    let api_request: IndexRequestBody = req.body_json().await.expect("unmarshal json");
    let state = req.state();

    let now = SystemTime::now();
    let datetime: DateTime<Utc> = now.into();
    let timestamp_str = datetime.format("%Y-%m-%d %H:%M:%S").to_string();
    let mut redis = state.pool.acquire().await?;
    let next_available = find_available_server(&mut state.possible_servers.clone(), &mut redis).await;
    match next_available {
        Some(server) => {
            embedding_request(
                state.pool.clone(),
                state.redis_expiration,
                timestamp_str,
                server,
                EmbedRequestBody {
                    model: state.embedding_model.clone(),
                    input: api_request.content.clone(),
                    stream: Some(false),
                },
                Uuid::nil(),
            )
            .await;

            return Ok(tide::Response::builder(tide::StatusCode::Ok)
                .content_type(mime::JSON)
                .body("{}")
                .build());
        }
        None => {
            return Ok(tide::Response::builder(tide::StatusCode::TooManyRequests)
                .content_type(mime::JSON)
                .body("{}")
                .build());
        }
    }
}

pub(crate) async fn ping(
    url: Url,
    username: String,
    password: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn_pool = SingleNodeConnectionPool::new(url);

    let transport = TransportBuilder::new(conn_pool)
        .auth(opensearch::auth::Credentials::Basic(username, password))
        .cert_validation(opensearch::cert::CertificateValidation::None)
        .build()?;

    let client = OpenSearch::new(transport);

    let response = client.ping().send().await?;

    println!("{:#?}", response.status_code());
    Ok(())
}

pub(crate) async fn create_index(
    client: &OpenSearch,
    index_name: String,
) -> Result<(), anyhow::Error> {
    let exists_response = client
        .indices()
        .exists(IndicesExistsParts::Index(&[&index_name]))
        .send()
        .await?;

    if exists_response.status_code().is_success() {
        println!("Index {} already exists, skipping creation.", index_name);
        return Ok(()); // Skip creation if index already exists
    }

    let index_body = json!({
        "settings": {
            "index": {
                "knn": true
            }
        },
        "mappings": {
            "properties": {
                "text": { "type": "text" },
                "embedding": {
                    "type": "knn_vector",
                    "dimension": 1024
                }
            }
        }
    });

    let response = client
        .indices()
        .create(IndicesCreateParts::Index(&index_name))
        .body(index_body)
        .send()
        .await?;
    let status = response.status_code();
    let text = response.text().await?;
    Ok(())
}

pub(crate) async fn insert_document(
    client: &OpenSearch,
    index_name: String,
    id: String,
    text: String,
    embedding: Vec<Vec<f64>>,
) -> Result<(), anyhow::Error> {
    let flattened_embedding: Vec<f64> = embedding.into_iter().flatten().collect();

    // Construct the document
    let doc = json!({
        "text": text,
        "embedding": flattened_embedding
    });

    let response = client
        .index(opensearch::IndexParts::IndexId(&index_name, id.as_str()))
        .body(doc)
        .send()
        .await?;
    println!("Insert Response: {:?}", &response.status_code());
    let r = response.text().await.expect("response").clone();
    println!("Insert Response {r}");
    Ok(())
}

pub(crate) async fn search_similar(
    client: &OpenSearch,
    index_name: String,
    query_vector: Vec<Vec<f64>>,
) -> Result<SearchResponse, anyhow::Error> {
    let flattened_embedding: Vec<f64> = query_vector.into_iter().flatten().collect();
    let query = json!({
        "size": 3,
        "query": {
            "knn": {
                "embedding": {
                    "vector": flattened_embedding,
                    "k": 25
                }
            }
        }
    });

    let response = client
        .search(SearchParts::Index(&[&index_name]))
        .body(query)
        .send()
        .await?;

    let body: SearchResponse = response.json().await?;
    Ok(body)
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SearchResponse {
    _shards: Shards,
    pub hits: Hits,
    pub timed_out: bool,
    pub took: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Shards {
    pub failed: u32,
    pub skipped: u32,
    pub successful: u32,
    pub total: u32,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Hits {
    pub hits: Vec<Hit>,
    pub max_score: Option<f32>,
    pub total: TotalHits,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Hit {
    pub _id: String,
    pub _index: String,
    pub _score: Option<f32>,
    pub _source: Source,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Source {
    pub embedding: Vec<f64>,
    pub text: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TotalHits {
    pub relation: String,
    pub value: u32,
}
