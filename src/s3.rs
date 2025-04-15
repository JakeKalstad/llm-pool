use anyhow::Result;
use minio_rsc::{client::ListObjectsArgs, provider::StaticProvider, Minio};
use reqwest::Client;
use tide::{Body, Request};

fn get_lifecycle_xml(days: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
    <LifecycleConfiguration>
        <Rule>
            <ID>ExpireObjectsAfter{}Days</ID>
            <Status>Enabled</Status>
            <Filter>
                <Prefix>your-object-prefix/</Prefix>
            </Filter>
            <Expiration>
                <Days>{}</Days>
            </Expiration>
        </Rule>
    </LifecycleConfiguration>"#,
        days, days
    )
}
async fn create_minio_client(s3_url: String, s3_user: String, s3_pass: String) -> Result<Minio> {
    let provider = StaticProvider::new(s3_user.as_str(), s3_pass.as_str(), None);
    let client = Minio::builder()
        .endpoint(s3_url)
        .provider(provider)
        .secure(false) // Make sure it's set to `true` if you're using HTTPS.
        .build()?;
    Ok(client)
}

pub async fn write_file_fs(
    s3_url: String,
    s3_user: String,
    s3_pass: String,
    expiration_days: i32,
    bucket_name: String,
    name: String,
    data: Vec<u8>,
) -> Result<()> {
    let client = create_minio_client(s3_url.clone(), s3_user.clone(), s3_pass.clone()).await?;
    let data = bytes::Bytes::from(data);
    client
        .put_object(&bucket_name, &name, data)
        .await
        .expect("Failed to upload file");
    set_lifecycle_policy(s3_url, s3_user, s3_pass, bucket_name, expiration_days).await?;
    Ok(())
}

pub async fn list_files(
    s3_url: String,
    s3_user: String,
    s3_pass: String,
    bucket_name: String,
) -> Result<Vec<minio_rsc::datatype::Object>> {
    let client = create_minio_client(s3_url, s3_user, s3_pass).await?;

    let mut stream = client.list_objects(bucket_name, <ListObjectsArgs as std::default::Default>::default());

    let results = stream.await.expect("get files");

    Ok(results.contents)
}

pub async fn get_file_fs(
    s3_url: String,
    s3_user: String,
    s3_pass: String,
    bucket_name: String,
    name: String,
) -> Result<Vec<u8>> {
    let client = create_minio_client(s3_url, s3_user, s3_pass).await?;
    let response = client.get_object(&bucket_name, &name).await?;
    Ok(response.bytes().await?.to_vec())
}

pub async fn set_lifecycle_policy(
    s3_url: String,
    s3_user: String,
    s3_pass: String,
    bucket_name: String,
    expiration_days: i32,
) -> Result<()> {
    let url = format!("{}/{}?lifecycle", s3_url, bucket_name);

    let client = Client::new();
    let res = client
        .put(&url)
        .basic_auth(s3_user, Some(s3_pass))
        .header("Content-Type", "application/xml")
        .body(get_lifecycle_xml(5))
        .send()
        .await?;

    if res.status().is_success() {
        println!(
            "Successfully set lifecycle policy for bucket {}",
            bucket_name
        );
    } else {
        eprintln!("Failed to set lifecycle policy: {:?}", res.status());
    }

    Ok(())
}

pub(crate) async fn serve_s3(req: Request<crate::State>) -> tide::Result {
    let state = req.state();
    let id: String = req.param("id")?.to_string();
    let file_data = get_file_fs(
        state.s3_url.clone(),
        state.s3_user.clone(),
        state.s3_pass.clone(),
        state.s3_bucket.clone(),
        id,
    )
    .await
    .map_err(|e| tide::Error::from_str(500, e)).expect("fail");

    let body = Body::from_bytes(file_data);
    Ok(tide::Response::builder(tide::StatusCode::Ok)
        .content_type("application/json")
        .body(body)
        .build())
}
