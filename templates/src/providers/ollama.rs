// External Crates
use super::{
    runtime::{ProviderImageResponse, ProviderUsage},
    ProviderError, ProviderKind,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use reqwest::ClientBuilder;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_IMAGE_SIZE: &str = "1024x1024";

#[derive(Deserialize, Debug)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
}

#[derive(Serialize, Debug)]
struct ImageGenerationRequest {
    model: String,
    prompt: String,
    n: u8,
    size: String,
    response_format: String,
}

#[derive(Deserialize, Debug)]
struct ImageGenerationResponse {
    data: Vec<ImageGenerationData>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

#[derive(Deserialize, Debug)]
struct ImageGenerationData {
    b64_json: String,
}

fn normalize_usage(
    usage: Option<Usage>,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
) -> Option<ProviderUsage> {
    if let Some(usage) = usage {
        return Some(ProviderUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            input_token_details: None,
            output_token_details: None,
        });
    }

    if prompt_eval_count.is_none() && eval_count.is_none() {
        return None;
    }

    Some(ProviderUsage {
        input_tokens: prompt_eval_count,
        output_tokens: eval_count,
        total_tokens: prompt_eval_count.and_then(|input| eval_count.map(|output| input + output)),
        input_token_details: None,
        output_token_details: None,
    })
}

fn normalize_images_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if let Some(index) = trimmed.find("/v1/") {
        return format!("{}/v1/images/generations", &trimmed[..index]);
    }

    if trimmed.ends_with("/v1") {
        format!("{trimmed}/images/generations")
    } else {
        format!("{trimmed}/v1/images/generations")
    }
}

pub async fn send_image_request(
    url: &String,
    model: &String,
    prompt: &str,
    timeout_in_sec: u64,
    token: &str,
) -> Result<ProviderImageResponse, ProviderError> {
    let request = ImageGenerationRequest {
        model: model.clone(),
        prompt: prompt.to_string(),
        n: 1,
        size: DEFAULT_IMAGE_SIZE.to_string(),
        response_format: "b64_json".to_string(),
    };

    let client = ClientBuilder::new()
        .timeout(Duration::from_secs(timeout_in_sec))
        .build()
        .map_err(|error| ProviderError::from_reqwest(ProviderKind::Ollama, error))?;

    let endpoint = normalize_images_url(url);
    let mut request_builder = client
        .post(endpoint.as_str())
        .header("Content-Type", "application/json");
    if !token.trim().is_empty() {
        request_builder = request_builder.header("Authorization", format!("Bearer {}", token));
    }

    let http_resp = request_builder
        .json(&request)
        .send()
        .await
        .map_err(|error| ProviderError::from_reqwest(ProviderKind::Ollama, error))?;

    let status = http_resp.status();
    let body_bytes = http_resp
        .bytes()
        .await
        .map_err(|error| ProviderError::from_reqwest(ProviderKind::Ollama, error))?;

    if !status.is_success() {
        let raw = String::from_utf8_lossy(&body_bytes);
        return Err(ProviderError::from_http_status(
            ProviderKind::Ollama,
            status,
            &raw,
        ));
    }

    let response: ImageGenerationResponse =
        serde_json::from_slice(&body_bytes).map_err(|error| {
            let raw = String::from_utf8_lossy(&body_bytes);
            ProviderError::invalid_response(
                ProviderKind::Ollama,
                format!("Failed to parse image-generation JSON: {error}\nRaw response:\n{raw}"),
            )
        })?;

    let encoded_image = response
        .data
        .first()
        .map(|image| image.b64_json.trim())
        .filter(|image| !image.is_empty())
        .ok_or_else(|| {
            ProviderError::invalid_response(
                ProviderKind::Ollama,
                "Image generation response did not include `data[0].b64_json`.",
            )
        })?;

    let bytes = BASE64_STANDARD.decode(encoded_image).map_err(|error| {
        ProviderError::invalid_response(
            ProviderKind::Ollama,
            format!("Failed to decode generated image bytes: {error}"),
        )
    })?;

    Ok(ProviderImageResponse {
        bytes,
        usage: normalize_usage(
            response.usage,
            response.prompt_eval_count,
            response.eval_count,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    #[tokio::test]
    async fn image_request_uses_images_endpoint_and_decodes_bytes() {
        let mut server = Server::new_async().await;
        let expected_bytes = b"fake-png";
        let encoded_image = BASE64_STANDARD.encode(expected_bytes);
        let _mock = server
            .mock("POST", "/v1/images/generations")
            .match_body(Matcher::PartialJson(serde_json::json!({
                "model": "x/flux2-klein:4b",
                "prompt": "draw a square",
                "n": 1,
                "size": "1024x1024",
                "response_format": "b64_json"
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"data":[{{"b64_json":"{}"}}]}}"#,
                encoded_image
            ))
            .create_async()
            .await;

        let url = format!("{}/v1/chat/completions", server.url());
        let model = "x/flux2-klein:4b".to_string();

        let image = send_image_request(&url, &model, "draw a square", 10, "")
            .await
            .expect("image request should decode");

        assert_eq!(image.bytes, expected_bytes);
        assert_eq!(image.usage, None);
    }
}
