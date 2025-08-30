use askama::Template;
use redis::AsyncCommands;

use crate::{
    api::{self, ApiResponse},
    s3, LLMServer, State,
};
use tide::{http::mime, Request};

pub async fn files(req: Request<State>) -> tide::Result {
    let state = req.state();
    let file_objs = s3::list_files(
        state.s3_url.clone(),
        state.s3_user.clone(),
        state.s3_pass.clone(),
        state.s3_bucket.clone(),
    )
    .await?;
    let home = FilesTemplate::new(file_objs);
    Ok(tide::Response::builder(tide::StatusCode::Ok)
        .content_type(mime::HTML)
        .body(home.render_string())
        .build())
}

pub async fn home(req: Request<State>) -> tide::Result {
    let mut redis = req.state().pool.get().await?;
    let queue_pattern = "queue-*".to_string();
    let prompt_pattern = "prompt-*".to_string();
    let mut queued_records = vec![];
    let mut prompt_records = vec![];

    let queued = api::scan_keys(&mut redis.clone(), queue_pattern)
        .await
        .unwrap_or_default();
    for q in queued {
        let response_data_id: String = redis.get(q).await.unwrap_or_default();
        let response_data_json: String = redis.get(response_data_id).await.unwrap_or_default();
        let response: ApiResponse =
            serde_json::from_str(&response_data_json).expect("response json is valid");
        queued_records.push(response);
    }
    let prompts = api::scan_keys(&mut redis.clone(), prompt_pattern)
        .await
        .unwrap_or_default();
    for p in prompts {
        let response_uuid: String = redis.get(p).await.unwrap_or_default();
        let response_data_json: String = redis.get(response_uuid).await.unwrap_or_default();
        if response_data_json.len() > 0 {
            let response: ApiResponse =
                serde_json::from_str(&response_data_json).expect("response json is valid");
            prompt_records.push(response);
        }
    }
    let possible_servers = req.state().possible_servers.clone();
    let home = HomeTemplate::new(possible_servers, queued_records, prompt_records);
    Ok(tide::Response::builder(tide::StatusCode::Ok)
        .content_type(mime::HTML)
        .body(home.render_string())
        .build())
}

pub async fn docs(_req: Request<State>) -> tide::Result {
    let docs = DocTemplate::new();
    Ok(tide::Response::builder(tide::StatusCode::Ok)
        .content_type(mime::HTML)
        .body(docs.render_string())
        .build())
}

#[derive(Template)]
#[template(path = "home.html")]
pub struct HomeTemplate {
    possible_servers: Vec<LLMServer>,
    queued: Vec<crate::api::ApiResponse>,
    prompts: Vec<crate::api::ApiResponse>,
}

impl HomeTemplate {
    pub fn new(
        possible_servers: Vec<LLMServer>,
        queued: Vec<crate::api::ApiResponse>,
        prompts: Vec<crate::api::ApiResponse>,
    ) -> Self {
        return Self {
            possible_servers,
            queued,
            prompts,
        };
    }

    pub fn render_string(&self) -> String {
        return self.render().unwrap();
    }
}

#[derive(Template)]
#[template(path = "files.html")]
pub struct FilesTemplate {
    files: Vec<minio_rsc::datatype::Object>,
}

impl FilesTemplate {
    pub fn new(files: Vec<minio_rsc::datatype::Object>) -> Self {
        return Self { files };
    }

    pub fn render_string(&self) -> String {
        return self.render().unwrap();
    }
}

#[derive(Template)]
#[template(path = "docs.html")]
pub struct DocTemplate {}

impl DocTemplate {
    pub fn new() -> Self {
        return Self {};
    }

    pub fn render_string(&self) -> String {
        return self.render().unwrap();
    }
}
