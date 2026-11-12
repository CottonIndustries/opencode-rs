//! Image generation protocols.

use super::sizing::{self, Limits, Size};
use super::{
    base_url, cost, decode_base64, download, encode_base64, headers, image_output, image_protocol,
    mime_for_format, post_json, post_multipart, post_response, value_f64, AuthStyle, ImageOutput,
    ImageProtocol, ImageRequest, ImageResult,
};
use crate::models::{Model, Provider};
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub async fn generate(provider: &Provider, model: &Model, request: &ImageRequest) -> Result<ImageResult> {
    match image_protocol(provider)? {
        ImageProtocol::OpenAi => openai(provider, model, request).await,
        ImageProtocol::Google => google(provider, model, request).await,
        ImageProtocol::Xai => xai(provider, model, request).await,
        ImageProtocol::Zai => zai(provider, model, request).await,
        ImageProtocol::Alibaba => alibaba(provider, model, request).await,
        ImageProtocol::Meta => meta(provider, model, request).await,
        ImageProtocol::Minimax => minimax(provider, model, request).await,
        ImageProtocol::Cloudflare => cloudflare(provider, model, request).await,
    }
}

fn protocol_headers(provider: &Provider, model: &Model, protocol: ImageProtocol) -> Vec<(String, String)> {
    let style = match protocol {
        ImageProtocol::Google => AuthStyle::Google,
        _ if provider.id.starts_with("azure") => AuthStyle::Azure,
        _ => AuthStyle::Bearer,
    };
    headers(provider, model, style)
}

fn require_single(request: &ImageRequest, name: &str) -> Result<()> {
    if request.n.unwrap_or(1) > 1 {
        bail!("{name} generates one image per request; call it once per image instead of n={}", request.n.unwrap_or(1));
    }
    Ok(())
}

fn size_field(size: Size) -> String {
    format!("{}x{}", size.width, size.height)
}

fn exact_size(request: &ImageRequest, limits: Limits) -> Result<Option<Size>> {
    if !sizing::is_requested(request.aspect.as_deref(), request.resolution.as_deref()) {
        return Ok(None);
    }
    let size = sizing::fit(request.aspect.as_deref(), request.resolution.as_deref(), limits);
    if let Some(missed) = sizing::missed(request.aspect.as_deref(), request.resolution.as_deref(), size) {
        bail!(
            "cannot render {missed}; the closest supported size is {}",
            sizing::describe(size)
        );
    }
    Ok(Some(size))
}

async fn openai(provider: &Provider, model: &Model, request: &ImageRequest) -> Result<ImageResult> {
    let base = base_url(provider, model, "https://api.openai.com/v1", ImageProtocol::OpenAi);
    let references = provider.id == "openrouter";
    let edit = !request.images.is_empty() && !references;
    let url = format!("{base}{}", if edit { "/images/edits" } else { "/images/generations" });
    let headers = protocol_headers(provider, model, ImageProtocol::OpenAi);
    let size = openai_size(&model.id, request);
    let mut body = json!({
        "model": model.id,
        "prompt": request.prompt,
    });
    if let Some(size) = &size {
        body["size"] = json!(size);
    }
    if let Some(n) = request.n {
        body["n"] = json!(n);
    }
    if references && !request.images.is_empty() {
        body["input_references"] = json!(request.images.iter().map(|image| {
            json!({ "type": "image_url", "image_url": { "url": format!("data:{};base64,{}", image.mime, image.data) } })
        }).collect::<Vec<_>>());
    }
    let response = if edit && request.images.iter().all(|image| !image.data.is_empty()) {
        let mut form = wreq::multipart::Form::new()
            .text("model", body["model"].as_str().unwrap_or_default().to_string())
            .text("prompt", request.prompt.clone());
        if let Some(size) = &size {
            form = form.text("size", size.clone());
        }
        if let Some(n) = request.n {
            form = form.text("n", n.to_string());
        }
        for (index, image) in request.images.iter().enumerate() {
            let part = wreq::multipart::Part::bytes(decode_base64(&image.data)?)
                .file_name(format!("image-{index}"))
                .mime_str(&image.mime)?;
            form = form.part("image[]", part);
        }
        post_multipart(&url, &headers, form).await?
    } else {
        if edit {
            body["images"] = json!(request.images.iter().map(|image| {
                json!({ "image_url": format!("data:{};base64,{}", image.mime, image.data) })
            }).collect::<Vec<_>>());
        }
        post_json(&url, &headers, &body).await?
    };
    let format = response.get("output_format").and_then(Value::as_str);
    let fallback = mime_for_format(format);
    let mut images = Vec::new();
    for item in response.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
        if let Some(output) = image_output(
            item.get("b64_json").and_then(Value::as_str),
            item.get("url").and_then(Value::as_str),
            &fallback,
        ) {
            images.push(output);
        } else if let Some(url) = item.get("url").and_then(Value::as_str) {
            images.push(download(url, &fallback).await?);
        }
    }
    if images.is_empty() {
        bail!("OpenAI Images returned no images");
    }
    let usage = response.get("usage").cloned().unwrap_or(Value::Null);
    let input_tokens = value_f64(&usage, "input_tokens");
    let output_tokens = value_f64(&usage, "output_tokens");
    Ok(ImageResult {
        images,
        input_tokens,
        output_tokens,
        cost: cost(model, input_tokens, output_tokens),
        native: Some(json!({
            "openai": {
                "output_format": format,
                "size": response.get("size"),
                "quality": response.get("quality"),
                "background": response.get("background"),
                "usage": usage,
            }
        })),
    })
}

fn openai_size(model: &str, request: &ImageRequest) -> Option<String> {
    if !sizing::is_requested(request.aspect.as_deref(), request.resolution.as_deref()) {
        return None;
    }
    let id = model.rsplit('/').next().unwrap_or(model);
    let sizes = if id.starts_with("dall-e-2") {
        Some(vec![
            Size { width: 256, height: 256 },
            Size { width: 512, height: 512 },
            Size { width: 1024, height: 1024 },
        ])
    } else if id.starts_with("dall-e-3") {
        Some(vec![
            Size { width: 1024, height: 1024 },
            Size { width: 1792, height: 1024 },
            Size { width: 1024, height: 1792 },
        ])
    } else if id.starts_with("gpt-image-1") || id.starts_with("chatgpt-image") {
        Some(vec![
            Size { width: 1024, height: 1024 },
            Size { width: 1536, height: 1024 },
            Size { width: 1024, height: 1536 },
        ])
    } else {
        None
    };
    let size = if let Some(sizes) = sizes {
        sizing::nearest(request.aspect.as_deref(), request.resolution.as_deref(), &sizes)
    } else {
        let limits = if id.starts_with("gpt-image-") {
            Limits {
                multiple: 16,
                max_edge: Some(3840),
                min_pixels: Some(655_360.0),
                max_pixels: Some(8_294_400.0),
                max_ratio: Some(3.0),
                ..Default::default()
            }
        } else {
            Limits { multiple: 16, ..Default::default() }
        };
        Some(sizing::fit(request.aspect.as_deref(), request.resolution.as_deref(), limits))
    };
    size.map(size_field)
}

async fn google(provider: &Provider, model: &Model, request: &ImageRequest) -> Result<ImageResult> {
    require_single(request, "Google Images")?;
    let base = base_url(provider, model, "https://generativelanguage.googleapis.com/v1beta", ImageProtocol::Google);
    let url = format!("{base}/models/{}:generateContent", model.id);
    let mut parts = vec![json!({ "text": request.prompt })];
    for image in &request.images {
        parts.push(json!({
            "inlineData": { "mimeType": image.mime, "data": image.data }
        }));
    }
    let resolution = request
        .resolution
        .as_deref()
        .map(|value| if value.eq_ignore_ascii_case("0.5K") { "512".to_string() } else { value.to_uppercase() });
    let mut config = json!({ "responseModalities": ["IMAGE"] });
    if request.aspect.is_some() || resolution.is_some() {
        config["imageConfig"] = json!({ "aspectRatio": request.aspect, "imageSize": resolution });
    }
    let body = json!({
        "contents": [{ "role": "user", "parts": parts }],
        "generationConfig": config,
    });
    let response = post_json(&url, &protocol_headers(provider, model, ImageProtocol::Google), &body).await?;
    let candidates = response.get("candidates").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut images = Vec::new();
    for candidate in &candidates {
        for part in candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let Some(inline) = part.get("inlineData") else { continue };
            let Some(data) = inline.get("data").and_then(Value::as_str) else { continue };
            let mime = inline.get("mimeType").and_then(Value::as_str).unwrap_or("image/png");
            images.push(ImageOutput { data: data.to_string(), mime: mime.to_string(), name: None });
        }
    }
    if images.is_empty() {
        let reasons: Vec<&str> = candidates
            .iter()
            .filter_map(|candidate| candidate.get("finishReason").and_then(Value::as_str))
            .collect();
        if reasons.iter().any(|reason| matches!(*reason, "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT")) {
            bail!("Google Images withheld its images (finish reasons: {})", reasons.join(", "));
        }
        bail!("Google Images returned no final images{}", if reasons.is_empty() { String::new() } else { format!(" (finish reasons: {})", reasons.join(", ")) });
    }
    let usage = response.get("usageMetadata").cloned().unwrap_or(Value::Null);
    let input_tokens = value_f64(&usage, "promptTokenCount");
    let output_tokens = value_f64(&usage, "candidatesTokenCount")
        .map(|value| value + value_f64(&usage, "thoughtsTokenCount").unwrap_or(0.0));
    Ok(ImageResult {
        images,
        input_tokens,
        output_tokens,
        cost: cost(model, input_tokens, output_tokens),
        native: Some(json!({ "google": {
            "modelVersion": response.get("modelVersion"),
            "responseId": response.get("responseId"),
            "promptFeedback": response.get("promptFeedback"),
            "usage": usage,
        } })),
    })
}

async fn xai(provider: &Provider, model: &Model, request: &ImageRequest) -> Result<ImageResult> {
    let base = base_url(provider, model, "https://api.x.ai/v1", ImageProtocol::Xai);
    let edit = !request.images.is_empty();
    let url = format!("{base}{}", if edit { "/images/edits" } else { "/images/generations" });
    let mut body = json!({
        "model": model.id,
        "prompt": request.prompt,
        "response_format": "b64_json",
        "n": request.n,
        "aspect_ratio": request.aspect,
        "resolution": request.resolution.as_deref().and_then(|value| {
            let tier = sizing::tier(value)?;
            Some(if tier < 1.5 { "1k" } else { "2k" })
        }),
    });
    if !request.images.is_empty() {
        let references: Vec<Value> = request
            .images
            .iter()
            .map(|image| json!({ "url": format!("data:{};base64,{}", image.mime, image.data), "type": "image_url" }))
            .collect();
        if references.len() == 1 {
            body["image"] = references[0].clone();
        } else {
            body["images"] = json!(references);
        }
    }
    let response = post_json(&url, &protocol_headers(provider, model, ImageProtocol::Xai), &body).await?;
    let mut images = Vec::new();
    for item in response.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
        let fallback = item.get("mime_type").and_then(Value::as_str).unwrap_or("image/png");
        if let Some(output) = image_output(
            item.get("b64_json").and_then(Value::as_str),
            item.get("url").and_then(Value::as_str),
            fallback,
        ) {
            images.push(output);
        } else if let Some(url) = item.get("url").and_then(Value::as_str) {
            images.push(download(url, fallback).await?);
        }
    }
    if images.is_empty() {
        bail!("xAI Images returned no images");
    }
    Ok(ImageResult {
        images,
        input_tokens: None,
        output_tokens: None,
        cost: None,
        native: Some(json!({ "xai": { "usage": response.get("usage") } })),
    })
}

async fn zai(provider: &Provider, model: &Model, request: &ImageRequest) -> Result<ImageResult> {
    let base = base_url(provider, model, "https://api.z.ai/api/paas/v4", ImageProtocol::Zai);
    let url = format!("{base}/images/generations");
    let limits = if model.id.starts_with("glm-image") {
        Limits { multiple: 32, min_edge: Some(1024), max_edge: Some(2048), max_pixels: Some((2f64).powi(22)), ..Default::default() }
    } else {
        Limits { multiple: 16, min_edge: Some(512), max_edge: Some(2048), max_pixels: Some((2f64).powi(21)), ..Default::default() }
    };
    let size = exact_size(request, limits)?;
    let mut body = json!({ "model": model.id, "prompt": request.prompt });
    if let Some(size) = size {
        body["size"] = json!(size_field(size));
    }
    let response = post_json(&url, &protocol_headers(provider, model, ImageProtocol::Zai), &body).await?;
    let mut images = Vec::new();
    for item in response.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
        if let Some(url) = item.get("url").and_then(Value::as_str) {
            images.push(download(url, "image/png").await?);
        }
    }
    if images.is_empty() {
        bail!("Z.ai Images returned no images");
    }
    Ok(ImageResult {
        images,
        input_tokens: None,
        output_tokens: None,
        cost: None,
        native: Some(json!({ "zai": {
            "created": response.get("created"),
            "id": response.get("id"),
            "requestID": response.get("request_id"),
            "contentFilter": response.get("content_filter"),
        } })),
    })
}

async fn alibaba(provider: &Provider, model: &Model, request: &ImageRequest) -> Result<ImageResult> {
    let base = base_url(
        provider,
        model,
        "https://dashscope-intl.aliyuncs.com/api/v1",
        ImageProtocol::Alibaba,
    );
    let url = format!("{base}/services/aigc/multimodal-generation/generation");
    let size = exact_size(
        request,
        Limits { multiple: 16, min_pixels: Some(512.0 * 512.0), max_pixels: Some(2048.0 * 2048.0), ..Default::default() },
    )?;
    let mut content: Vec<Value> = request
        .images
        .iter()
        .map(|image| json!({ "image": format!("data:{};base64,{}", image.mime, image.data) }))
        .collect();
    content.push(json!({ "text": request.prompt }));
    let mut parameters = json!({ "n": request.n });
    if let Some(size) = size {
        parameters["size"] = json!(format!("{}*{}", size.width, size.height));
    }
    let body = json!({
        "model": model.id,
        "input": { "messages": [{ "role": "user", "content": content }] },
        "parameters": parameters,
    });
    let response = post_json(&url, &protocol_headers(provider, model, ImageProtocol::Alibaba), &body).await?;
    let mut images = Vec::new();
    for choice in response
        .pointer("/output/choices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        for item in choice
            .pointer("/message/content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(url) = item.get("image").and_then(Value::as_str) {
                images.push(download(url, "image/png").await?);
            }
        }
    }
    if images.is_empty() {
        bail!("Alibaba Images returned no images");
    }
    Ok(ImageResult {
        images,
        input_tokens: None,
        output_tokens: None,
        cost: None,
        native: Some(json!({ "alibaba": {
            "requestID": response.get("request_id"),
            "usage": response.get("usage"),
        } })),
    })
}

async fn meta(provider: &Provider, model: &Model, request: &ImageRequest) -> Result<ImageResult> {
    let base = base_url(provider, model, "https://api.meta.ai/v1", ImageProtocol::Meta);
    let edit = !request.images.is_empty();
    let url = format!("{base}/images/{}", if edit { "edits" } else { "generations" });
    let mut body = json!({
        "model": model.id,
        "prompt": request.prompt,
        "n": request.n,
        "output_format": "webp",
    });
    if edit {
        body["images"] = json!(request.images.iter().map(|image| {
            json!({ "image_url": format!("data:{};base64,{}", image.mime, image.data) })
        }).collect::<Vec<_>>());
    }
    if request.aspect.is_some() {
        let hint = sizing::fit(request.aspect.as_deref(), None, Limits { multiple: 16, ..Default::default() });
        body["size"] = json!(size_field(hint));
    }
    let response = post_json(&url, &protocol_headers(provider, model, ImageProtocol::Meta), &body).await?;
    let format = response.get("output_format").and_then(Value::as_str).unwrap_or("webp");
    let fallback = mime_for_format(Some(format));
    let mut images = Vec::new();
    for item in response.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
        if let Some(output) = image_output(item.get("b64_json").and_then(Value::as_str), item.get("url").and_then(Value::as_str), &fallback) {
            images.push(output);
        } else if let Some(url) = item.get("url").and_then(Value::as_str) {
            images.push(download(url, &fallback).await?);
        }
    }
    if images.is_empty() {
        bail!("Meta Images returned no images");
    }
    let usage = response.get("usage").cloned().unwrap_or(Value::Null);
    let input_tokens = value_f64(&usage, "input_tokens");
    let output_tokens = value_f64(&usage, "output_tokens");
    Ok(ImageResult {
        images,
        input_tokens,
        output_tokens,
        cost: cost(model, input_tokens, output_tokens),
        native: Some(json!({ "meta": { "outputFormat": format } })),
    })
}

const MINIMAX_PRESETS: &[(&str, Size)] = &[
    ("1:1", Size { width: 1024, height: 1024 }),
    ("16:9", Size { width: 1280, height: 720 }),
    ("4:3", Size { width: 1152, height: 864 }),
    ("3:2", Size { width: 1248, height: 832 }),
    ("2:3", Size { width: 832, height: 1248 }),
    ("3:4", Size { width: 864, height: 1152 }),
    ("9:16", Size { width: 720, height: 1280 }),
    ("21:9", Size { width: 1344, height: 576 }),
];

async fn minimax(provider: &Provider, model: &Model, request: &ImageRequest) -> Result<ImageResult> {
    if request.images.len() > 1 {
        bail!("MiniMax Images takes one subject reference image");
    }
    let base = base_url(provider, model, "https://api.minimax.io/v1", ImageProtocol::Minimax);
    let url = format!("{base}/image_generation");
    let mut body = json!({
        "model": model.id,
        "prompt": request.prompt,
        "n": request.n,
        "response_format": "base64",
    });
    if let Some(image) = request.images.first() {
        body["subject_reference"] = json!([{
            "type": "character",
            "image_file": format!("data:{};base64,{}", image.mime, image.data),
        }]);
    }
    if sizing::is_requested(request.aspect.as_deref(), request.resolution.as_deref()) {
        let aspect = request.aspect.as_deref().unwrap_or("1:1");
        let preset = MINIMAX_PRESETS.iter().find(|(name, _)| *name == aspect);
        let tier = request.resolution.as_deref().and_then(sizing::tier);
        if let Some((name, _)) = preset.filter(|_| tier.is_none() || tier == Some(1.0)) {
            body["aspect_ratio"] = json!(name);
        } else if model.id == "image-01" {
            let size = exact_size(
                request,
                Limits { multiple: 8, min_edge: Some(512), max_edge: Some(2048), ..Default::default() },
            )?;
            if let Some(size) = size {
                body["width"] = json!(size.width);
                body["height"] = json!(size.height);
            }
        } else {
            let available: Vec<Size> = MINIMAX_PRESETS
                .iter()
                .filter(|(name, _)| model.id == "image-01" || *name != "21:9")
                .map(|(_, size)| *size)
                .collect();
            if let Some(closest) = sizing::nearest(request.aspect.as_deref(), request.resolution.as_deref(), &available) {
                bail!(
                    "{} cannot render the requested size; the closest supported size is {}",
                    model.id,
                    sizing::describe(closest)
                );
            }
        }
    }
    let response = post_json(&url, &protocol_headers(provider, model, ImageProtocol::Minimax), &body).await?;
    if let Some(status) = response.get("base_resp") {
        let code = status.get("status_code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 {
            bail!(
                "MiniMax Images failed ({code}): {}",
                status.get("status_msg").and_then(Value::as_str).unwrap_or("unknown error")
            );
        }
    }
    let mut images = Vec::new();
    if let Some(data) = response.pointer("/data/image_base64").and_then(Value::as_array) {
        for item in data {
            if let Some(data) = item.as_str() {
                images.push(ImageOutput { data: data.to_string(), mime: "image/png".into(), name: None });
            }
        }
    }
    if let Some(urls) = response.pointer("/data/image_urls").and_then(Value::as_array) {
        for item in urls {
            if let Some(url) = item.as_str() {
                images.push(download(url, "image/png").await?);
            }
        }
    }
    if images.is_empty() {
        bail!("MiniMax Images returned no images");
    }
    Ok(ImageResult {
        images,
        input_tokens: None,
        output_tokens: None,
        cost: None,
        native: Some(json!({ "minimax": {
            "id": response.get("id"),
            "successCount": response.pointer("/metadata/success_count"),
            "failedCount": response.pointer("/metadata/failed_count"),
        } })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ModelKind, Provider};
    use std::collections::BTreeMap;

    fn test_provider(id: &str, npm: &str, api: String) -> Provider {
        let mut options = crate::config::ProviderOptions::default();
        options.base_url = Some(api);
        options.api_key = Some("test".into());
        Provider {
            id: id.into(),
            name: id.into(),
            npm: npm.into(),
            api: None,
            env: vec![],
            key: Some("test".into()),
            options,
            models: BTreeMap::new(),
            media_models: BTreeMap::new(),
        }
    }

    fn test_model(provider_id: &str, id: &str, kind: ModelKind) -> crate::models::Model {
        crate::models::Model {
            id: id.into(),
            provider_id: provider_id.into(),
            kind,
            name: id.into(),
            family: String::new(),
            reasoning: false,
            reasoning_options: vec![],
            tool_call: false,
            attachment: true,
            temperature: false,
            context: 0,
            output: 0,
            cost_input: 0.0,
            cost_output: 0.0,
            cost_cache_read: 0.0,
            cost_cache_write: 0.0,
            input_modalities: vec!["text".into(), "image".into()],
            output_modalities: vec!["image".into()],
            interleaved_field: None,
            release_date: String::new(),
            status: None,
            npm: Some("@ai-sdk/openai".into()),
            api: None,
            shape: None,
            options: Value::Null,
            headers: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn openai_generation_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            assert!(request.contains("POST"));
            assert!(request.contains("/images/generations"));
            assert!(request.contains("\"prompt\":\"a cat\""));
            let body = "{\"data\":[{\"b64_json\":\"aGk=\"}],\"output_format\":\"png\",\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let provider = test_provider("openai", "@ai-sdk/openai", format!("http://{address}/v1"));
        let model = test_model("openai", "gpt-image-2", ModelKind::Image);
        let request = ImageRequest {
            prompt: "a cat".into(),
            images: vec![],
            n: Some(1),
            aspect: None,
            resolution: None,
        };
        let result = generate(&provider, &model, &request).await.unwrap();
        assert_eq!(result.images[0].data, "aGk=");
        assert_eq!(result.images[0].mime, "image/png");
        assert_eq!(result.input_tokens, Some(10.0));
        server.await.unwrap();
    }

    async fn serve(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}"), server)
    }

    fn request() -> ImageRequest {
        ImageRequest {
            prompt: "a cat".into(),
            images: vec![],
            n: Some(1),
            aspect: Some("1:1".into()),
            resolution: Some("1K".into()),
        }
    }

    #[tokio::test]
    async fn google_round_trip() {
        let (base, server) = serve(
            "{\"candidates\":[{\"content\":{\"parts\":[{\"inlineData\":{\"mimeType\":\"image/png\",\"data\":\"aGk=\"}}]}}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":4}}",
        )
        .await;
        let provider = test_provider("google", "@ai-sdk/google", format!("{base}/v1beta"));
        let result = generate(&provider, &test_model("google", "gemini-image", ModelKind::Image), &request())
            .await
            .unwrap();
        assert_eq!(result.images[0].data, "aGk=");
        let request = server.await.unwrap();
        assert!(request.contains("/models/gemini-image:generateContent"));
        assert!(request.contains("\"responseModalities\":[\"IMAGE\"]"));
    }

    #[tokio::test]
    async fn xai_round_trip() {
        let (base, server) = serve(
            "{\"data\":[{\"b64_json\":\"aGk=\",\"mime_type\":\"image/png\"}],\"usage\":{\"cost_in_usd_ticks\":1}}",
        )
        .await;
        let provider = test_provider("xai", "@ai-sdk/xai", format!("{base}/v1"));
        let result = generate(&provider, &test_model("xai", "grok-image", ModelKind::Image), &request())
            .await
            .unwrap();
        assert_eq!(result.images[0].data, "aGk=");
        let request = server.await.unwrap();
        assert!(request.contains("/images/generations"));
        assert!(request.contains("\"aspect_ratio\":\"1:1\""));
    }

    #[tokio::test]
    async fn zai_round_trip() {
        let (base, server) = serve("{\"data\":[{\"url\":\"data:image/png;base64,aGk=\"}]}").await;
        let provider = test_provider("zai", "@ai-sdk/openai-compatible", format!("{base}/api/paas/v4"));
        let result = generate(&provider, &test_model("zai", "glm-image", ModelKind::Image), &request())
            .await
            .unwrap();
        assert_eq!(result.images[0].data, "aGk=");
        let request = server.await.unwrap();
        assert!(request.contains("/images/generations"));
        assert!(request.contains("\"size\":\"1024x1024\""));
    }

    #[tokio::test]
    async fn alibaba_round_trip() {
        let (base, server) = serve(
            "{\"output\":{\"choices\":[{\"message\":{\"content\":[{\"image\":\"data:image/png;base64,aGk=\"}]}}]},\"request_id\":\"r\"}",
        )
        .await;
        let provider = test_provider("alibaba", "@ai-sdk/openai-compatible", format!("{base}/api/v1"));
        let result = generate(&provider, &test_model("alibaba", "qwen-image", ModelKind::Image), &request())
            .await
            .unwrap();
        assert_eq!(result.images[0].data, "aGk=");
        let request = server.await.unwrap();
        assert!(request.contains("/services/aigc/multimodal-generation/generation"));
        assert!(request.contains("\"size\":\"1024*1024\""));
    }

    #[tokio::test]
    async fn meta_round_trip() {
        let (base, server) = serve(
            "{\"data\":[{\"b64_json\":\"aGk=\"}],\"output_format\":\"webp\",\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}",
        )
        .await;
        let provider = test_provider("meta", "@ai-sdk/openai", format!("{base}/v1"));
        let result = generate(&provider, &test_model("meta", "meta-image", ModelKind::Image), &request())
            .await
            .unwrap();
        assert_eq!(result.images[0].mime, "image/webp");
        let request = server.await.unwrap();
        assert!(request.contains("/images/generations"));
    }

    #[tokio::test]
    async fn minimax_round_trip() {
        let (base, server) = serve("{\"data\":{\"image_base64\":[\"aGk=\"]},\"base_resp\":{\"status_code\":0}}").await;
        let provider = test_provider("minimax", "@ai-sdk/anthropic", format!("{base}/v1"));
        let result = generate(&provider, &test_model("minimax", "image-01", ModelKind::Image), &request())
            .await
            .unwrap();
        assert_eq!(result.images[0].data, "aGk=");
        let request = server.await.unwrap();
        assert!(request.contains("/image_generation"));
        assert!(request.contains("\"aspect_ratio\":\"1:1\""));
    }

    #[tokio::test]
    async fn cloudflare_round_trip() {
        let (base, server) = serve("{\"result\":{\"image\":\"aGk=\"}}").await;
        let provider = test_provider("cloudflare-workers-ai", "@ai-sdk/openai-compatible", format!("{base}/ai/run"));
        let result = generate(
            &provider,
            &test_model("cloudflare-workers-ai", "@cf/model", ModelKind::Image),
            &request(),
        )
        .await
        .unwrap();
        assert_eq!(result.images[0].data, "aGk=");
        let request = server.await.unwrap();
        assert!(request.contains("/ai/run/@cf/model"));
    }

    #[test]
    fn openai_tiers_and_limits() {
        let request = ImageRequest {
            prompt: "x".into(),
            images: vec![],
            n: None,
            aspect: Some("16:9".into()),
            resolution: Some("1K".into()),
        };
        assert_eq!(openai_size("gpt-image-1", &request).as_deref(), Some("1536x1024"));
        let size = openai_size("gpt-image-2", &request).unwrap();
        assert!(size.starts_with("1536x") || size.contains('x'));
    }

    #[test]
    fn minimax_presets_exist() {
        assert!(MINIMAX_PRESETS.iter().any(|(name, _)| *name == "21:9"));
    }
}

async fn cloudflare(provider: &Provider, model: &Model, request: &ImageRequest) -> Result<ImageResult> {
    require_single(request, "Cloudflare Workers AI Images")?;
    let base = base_url(provider, model, "https://api.cloudflare.com/client/v4", ImageProtocol::Cloudflare);
    let url = format!("{base}/{}", model.id.trim_start_matches('/'));
    let size = exact_size(
        request,
        Limits { multiple: 16, min_edge: Some(256), max_edge: Some(2048), ..Default::default() },
    )?;
    let multipart = model.id.starts_with("@cf/black-forest-labs/flux-2-");
    let headers = protocol_headers(provider, model, ImageProtocol::Cloudflare);
    let response = if multipart {
        let mut form = wreq::multipart::Form::new().text("prompt", request.prompt.clone());
        if let Some(size) = size {
            form = form.text("width", size.width.to_string()).text("height", size.height.to_string());
        }
        for (index, image) in request.images.iter().enumerate() {
            let part = wreq::multipart::Part::bytes(decode_base64(&image.data)?)
                .file_name(format!("image_{index}"))
                .mime_str(&image.mime)?;
            form = form.part(format!("input_image_{index}"), part);
        }
        let mut builder = crate::http::client().post(&url).multipart(form);
        for (key, value) in &headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        crate::http::send(builder).await?
    } else {
        if request.images.len() > 1 {
            bail!("{} takes one input image as image_b64", model.id);
        }
        let mut body = json!({ "prompt": request.prompt });
        if let Some(size) = size {
            body["width"] = json!(size.width);
            body["height"] = json!(size.height);
        }
        if let Some(image) = request.images.first() {
            body["image_b64"] = json!(image.data);
        }
        post_response(&url, &headers, &body).await?
    };
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await?;
        bail!("Cloudflare Workers AI Images failed ({status}): {}", crate::util::truncate_chars(text.trim(), 500));
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !content_type.starts_with("application/json") {
        let mime = content_type.split(';').next().unwrap_or("image/png").trim().to_string();
        let bytes = response.bytes().await?;
        return Ok(ImageResult {
            images: vec![ImageOutput { data: encode_base64(&bytes), mime, name: None }],
            ..Default::default()
        });
    }
    let text = response.text().await?;
    let value: Value = serde_json::from_str(&text)?;
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    let Some(image) = result.get("image").and_then(Value::as_str) else {
        bail!("Cloudflare Workers AI Images returned no image");
    };
    Ok(ImageResult {
        images: vec![ImageOutput { data: image.to_string(), mime: "image/png".into(), name: None }],
        native: Some(json!({ "cloudflare-workers-ai": { "usage": result.get("usage") } })),
        ..Default::default()
    })
}
