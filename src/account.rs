//! OpenCode Console account usage limits.

use crate::auth::Credential;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

const DEFAULT_SERVER: &str = "https://opencode.ai/console";

struct ConsoleCredential {
    token: String,
    server: String,
    org: String,
}

fn string_metadata(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(key).and_then(Value::as_str).map(String::from))
}

fn credential() -> Option<ConsoleCredential> {
    let all = crate::auth::all();
    let value = all.get("opencode")?;
    let (token, server, org) = match value {
        Credential::Api { key, metadata: data } => {
            let data = data.as_ref()?;
            let server = ["server"].iter().find_map(|key| data.get(*key).cloned());
            let org = ["orgID", "orgId", "org_id"].iter().find_map(|key| data.get(*key).cloned());
            (key.clone(), server, org)
        }
        Credential::Oauth { access, metadata: data, .. } => {
            let data = data.as_ref()?;
            (access.clone(), string_metadata(data, &["server"]), string_metadata(data, &["orgID", "orgId", "org_id"]))
        }
        Credential::Wellknown { .. } => return None,
    };
    let org = org?;
    let server = server.unwrap_or_else(|| DEFAULT_SERVER.to_string());
    let server = server.replace("https://console.opencode.ai", DEFAULT_SERVER);
    Some(ConsoleCredential { token, server, org })
}

fn millis(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|date| date.timestamp_millis())
        .unwrap_or(0)
}

async fn get_json(url: &str, token: &str, org: &str) -> Result<(u16, Value)> {
    let response = crate::http::send(
        crate::http::client()
            .get(url)
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .header("x-org-id", org)
            .timeout(std::time::Duration::from_secs(15)),
    )
    .await?;
    let status = response.status().as_u16();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    Ok((status, value))
}

fn budget(value: &Value) -> Value {
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .map(|model| {
                    let mut out = json!({
                        "providerID": model.get("providerID").cloned().unwrap_or(Value::Null),
                        "modelID": model.get("modelID").cloned().unwrap_or(Value::Null),
                        "name": model.get("name").cloned().unwrap_or(Value::Null),
                    });
                    if let Some(spent) = model.get("spentDollars") {
                        out["spent"] = spent.clone();
                    }
                    out
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let vendors = value
        .get("vendors")
        .and_then(Value::as_array)
        .map(|vendors| {
            vendors
                .iter()
                .filter_map(|vendor| vendor.get("name").and_then(Value::as_str).map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut out = json!({
        "id": value.get("ruleID").cloned().unwrap_or(Value::Null),
        "limit": value.get("limitDollars").cloned().unwrap_or(json!(0)),
        "spent": value.get("spentDollars").cloned().unwrap_or(json!(0)),
        "scope": value.get("scope").cloned().unwrap_or(Value::Null),
        "per": value.get("per").cloned().unwrap_or(Value::Null),
        "coverage": value.get("coverage").cloned().unwrap_or(Value::Null),
        "models": models,
        "vendors": vendors,
        "picked": value.get("picked").cloned().unwrap_or(json!(false)),
        "shared": value.get("shared").cloned().unwrap_or(json!(false)),
        "resetsAt": millis(value.get("resetsAt")),
    });
    if let Some(price) = value.get("inputPriceAtLeast") {
        out["inputPriceAtLeast"] = price.clone();
    }
    out
}

fn meter(value: &Value) -> Value {
    json!({
        "percent": value.get("percent").cloned().unwrap_or(json!(0)),
        "limited": value.get("status").and_then(Value::as_str) == Some("rate-limited"),
        "resetsAt": millis(value.get("resetsAt")),
    })
}

pub async fn usage_limits() -> Result<Option<Value>> {
    let Some(credential) = credential() else { return Ok(None) };
    let origin = url::Url::parse(&credential.server)
        .map(|url| format!("{}://{}", url.scheme(), url.authority()))
        .unwrap_or_else(|_| "https://opencode.ai".to_string());
    let budgets_url = format!("{}/api/policies/current-budgets", credential.server);
    let go_url = format!("{origin}/zen/go/v1/usage");
    let (budgets, go) = tokio::join!(
        get_json(&budgets_url, &credential.token, &credential.org),
        get_json(&go_url, &credential.token, &credential.org),
    );
    let (budget_status, budgets) = budgets?;
    let budgets = if budget_status == 403 {
        Vec::new()
    } else if (200..300).contains(&budget_status) {
        budgets
            .as_array()
            .map(|items| items.iter().map(budget).collect::<Vec<_>>())
            .unwrap_or_default()
    } else {
        return Err(anyhow!("OpenCode Console budgets are unavailable: HTTP {budget_status}"));
    };
    let go = match go {
        Ok((403, _)) | Err(_) => None,
        Ok((status, value)) if (200..300).contains(&status) => value.get("usage").map(|usage| {
            json!({
                "rolling": meter(&usage["rolling"]),
                "weekly": meter(&usage["weekly"]),
                "monthly": meter(&usage["monthly"]),
            })
        }),
        Ok((status, _)) => return Err(anyhow!("OpenCode Go usage is unavailable: HTTP {status}")),
    };
    let mut info = json!({
        "workspace": {
            "id": credential.org,
            "usageURL": format!("{}/{}/activity/overview", credential.server, credential.org),
        },
        "budgets": budgets,
    });
    if let Some(go) = go {
        info["go"] = go;
    }
    Ok(Some(info))
}
