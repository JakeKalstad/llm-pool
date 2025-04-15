use std::str::FromStr;

use ::opensearch::OpenSearch;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tide::{Body, Request, Response, StatusCode};
use tokio::{
    fs,
    io::{self, AsyncReadExt},
};
mod api;
mod home;
mod opensearch_api;
mod s3;
use redis;
use redis_pool::RedisPool;

#[derive(Clone)]
pub struct State {
    pool: RedisPool<redis::Client, redis::aio::MultiplexedConnection>,
    possible_servers: Vec<LLMServer>,
    redis_expiration: u64,
    s3_url: String,
    s3_user: String,
    s3_pass: String,
    s3_expiration: i32,
    s3_bucket: String,
    opensearch_user: String,
    opensearch_password: String,
    opensearch_servers: Vec<OSServer>,
    opensearch_index: String,
    opensearch_clients: Vec<OpenSearch>,
    embedding_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMServer {
    url: String,
    weight: i32,
    max_concurrent: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OSServer {
    url: String,
}

pub async fn serve_dir(req: Request<State>) -> tide::Result {
    let f_local_path = "./assets".to_string() + req.url().path().replace("/fs", "").as_str();
    match Body::from_file(f_local_path.clone()).await {
        Ok(body) => {
            let mut builder = Response::builder(StatusCode::Ok).body(body);
            if f_local_path.clone().contains(".css") || f_local_path.contains(".js") {
                builder = builder.header("Cache-Control", "max-age=31536000, immutable");
            }
            Ok(builder.build())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Response::new(StatusCode::NotFound)),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct AppConfig {
    redis_url: String,
    s3_url: String,
    s3_user: String,
    s3_pass: String,
    s3_expiration: i32,
    s3_prompt_bucket: String,
    redis_expiration: u64,
    servers: Vec<LLMServer>,
    opensearch_user: String,
    opensearch_password: String,
    opensearch_servers: Vec<OSServer>,
    opensearch_index: String,
    embedding_model: String,
}

async fn read_config(path: &str) -> Result<AppConfig, Box<dyn std::error::Error>> {
    let mut file = fs::File::open(path).await.expect("file exists");
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .await
        .expect("String from config readable");
    let config: AppConfig = serde_json::from_str(&contents)?;
    Ok(config)
}

#[tokio::main]
async fn main() -> tide::Result<()> {
    let config = read_config("config.json").await.unwrap();
    let redis_url = config.redis_url;
    let client = redis::Client::open(redis_url).expect("Error while testing the connection");
    let pool = RedisPool::from(client);
    let p = pool.clone();
    let mut opensearch_clients = vec![];
    for os in config.opensearch_servers.clone() {
        let _x = opensearch_api::ping(
            Url::from_str(&os.url).expect("Url works"),
            config.opensearch_user.clone(),
            config.opensearch_password.clone(),
        )
        .await
        .unwrap();
        let client = opensearch_api::new(
            Url::from_str(&os.url).expect("Url works"),
            config.opensearch_user.clone(),
            config.opensearch_password.clone(),
        )
        .expect("client created");
        opensearch_api::create_index(&client, config.opensearch_index.clone())
            .await
            .unwrap();
        opensearch_clients.push(client);
    }

    let state = State {
        pool: p,
        possible_servers: config.servers,
        opensearch_clients: opensearch_clients.clone(),
        redis_expiration: config.redis_expiration,
        s3_url: config.s3_url.clone(),
        s3_user: config.s3_user.clone(),
        s3_pass: config.s3_pass.clone(),
        s3_expiration: config.s3_expiration.clone(),
        s3_bucket: config.s3_prompt_bucket.clone(),
        opensearch_user: config.opensearch_user.clone(),
        opensearch_password: config.opensearch_password.clone(),
        opensearch_servers: config.opensearch_servers.clone(),
        opensearch_index: config.opensearch_index.clone(),
        embedding_model: config.embedding_model.clone(),
    };
    let servers = state.possible_servers.clone();
    let s3_url = config.s3_url.clone();
    let s3_user = config.s3_user.clone();
    let s3_pass = config.s3_pass.clone();
    let s3_prompt_bucket = config.s3_prompt_bucket.clone();
    let opensearch_user = config.opensearch_user.clone();
    let opensearch_password = config.opensearch_password.clone();
    let opensearch_servers = config.opensearch_servers.clone();
    let opensearch_index = config.opensearch_index.clone();
    let embedding_model = config.embedding_model.clone();

    tokio::spawn(async move {
        loop {
            println!("Running periodic task...");
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            api::pop_queue(
                pool.clone(),
                &servers,
                config.redis_expiration,
                s3_url.to_string(),
                s3_user.to_string(),
                s3_pass.to_string(),
                config.s3_expiration.clone(),
                s3_prompt_bucket.to_string(),
                &opensearch_servers,
                opensearch_user.clone(),
                opensearch_password.clone(),
                opensearch_index.clone(),
                opensearch_clients.clone(),
                embedding_model.clone(),
            )
            .await;
        }
    });

    let mut app = tide::with_state(state);

    app.at("/").get(home::home);
    app.at("/docs").get(home::docs);
    app.at("/files").get(home::files);
    app.at("/index").post(opensearch_api::index);
    // app.at("/index_file").post(opensearch_api::index_file);
    app.at("/api/generate").post(api::request);
    app.at("/:id").get(api::get);
    app.at("/fs/*").get(serve_dir);
    app.at("/file/:id").get(s3::serve_s3);

    app.listen("0.0.0.0:8080").await?;
    Ok(())
}
