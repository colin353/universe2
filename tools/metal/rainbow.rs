use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

pub fn parse(name: &str) -> std::io::Result<(&str, &str)> {
    let components: Vec<_> = name.split(':').collect();
    if components.len() != 2 {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }

    Ok((components[0], components[1]))
}

pub async fn async_resolve(name: &str, tag: &str) -> Option<String> {
    let uri: hyper::Uri =
        format!("https://storage.googleapis.com/rainbow-binaries/{name}/tags/{tag}")
            .parse()
            .ok()?;

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .build();
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build(https);
    let req = hyper::Request::builder()
        .method("GET")
        .uri(uri)
        .body(Full::new(Bytes::new()))
        .ok()?;
    let res = client.request(req).await.ok()?;
    if !res.status().is_success() {
        return None;
    }

    let bytes = res.into_body().collect().await.ok()?.to_bytes();
    String::from_utf8(bytes.to_vec()).ok()
}
