//! `question`: ask the user structured questions through the form store.

use super::{Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct QuestionTool;

#[async_trait]
impl Tool for QuestionTool {
    fn name(&self) -> &str {
        "question"
    }

    fn description(&self) -> String {
        "Use this tool when you need to ask the user questions during execution. This allows you to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take.

Usage notes:
- A \"Type your own answer\" option is added automatically; don't include a separate option for free form answers
- Set `multiple: true` to allow selecting more than one option
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label"
            .into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string", "description": "Complete question" },
                            "header": { "type": "string", "description": "Very short label (max 30 chars)" },
                            "options": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "Display text (1-5 words, concise)" },
                                        "description": { "type": "string", "description": "Explanation of choice" }
                                    },
                                    "required": ["label", "description"],
                                    "additionalProperties": false
                                }
                            },
                            "multiple": { "type": "boolean", "description": "Allow selecting multiple choices" }
                        },
                        "required": ["question", "header", "options"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let questions = input
            .get("questions")
            .and_then(Value::as_array)
            .filter(|questions| !questions.is_empty())
            .ok_or_else(|| anyhow!("The questions parameter must be a non-empty array"))?
            .clone();
        ctx.ask(
            "question",
            &["*".into()],
            &[],
            "Ask the user a question".into(),
            json!({ "questions": questions }),
        )
        .await?;
        if *ctx.app.ask_mode.lock().unwrap() != crate::app::AskMode::Interactive {
            return Err(anyhow!("The user dismissed this question"));
        }

        let fields: Vec<Value> = questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let options = question
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .map(|option| {
                                json!({
                                    "value": option.get("label").cloned().unwrap_or(Value::Null),
                                    "label": option.get("label").cloned().unwrap_or(Value::Null),
                                    "description": option.get("description").cloned().unwrap_or(Value::Null),
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                json!({
                    "key": format!("q{index}"),
                    "title": question.get("header").and_then(Value::as_str).unwrap_or("Question"),
                    "description": question.get("question").and_then(Value::as_str).unwrap_or(""),
                    "type": if question.get("multiple").and_then(Value::as_bool).unwrap_or(false) { "multiselect" } else { "string" },
                    "options": options,
                    "custom": true,
                })
            })
            .collect();

        let form = ctx.app.forms.create(
            &ctx.session_id,
            "Questions",
            json!({
                "kind": "question",
                "tool": { "messageID": ctx.message_id, "id": ctx.call_id },
            }),
            Value::Array(fields),
        );
        ctx.app.plugins.emit(json!({
            "id": crate::util::id("evt"),
            "type": "form.created",
            "properties": { "form": form }
        }));

        let outcome = tokio::select! {
            result = ctx.app.forms.wait(&form.id) => result?,
            _ = ctx.cancel.cancelled() => {
                let _ = ctx.app.forms.cancel_with_message(&form.id, None);
                return Err(anyhow!("The user dismissed this question"));
            }
        };
        let replies = match outcome {
            crate::forms::Outcome::Replied(replies) => replies,
            crate::forms::Outcome::Cancelled => {
                return Err(anyhow!("The user dismissed this question"));
            }
        };
        let answers: Vec<Value> = questions
            .iter()
            .enumerate()
            .map(|(index, _)| {
                match replies.get(format!("q{index}")) {
                    Some(Value::Array(values)) => Value::Array(values.clone()),
                    Some(Value::String(value)) => json!([value]),
                    Some(Value::Null) | None => json!([]),
                    Some(value) => json!([value.to_string()]),
                }
            })
            .collect();
        let formatted = questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let question = question.get("question").and_then(Value::as_str).unwrap_or("");
                let answer = answers
                    .get(index)
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "Unanswered".into());
                format!("\"{question}\"=\"{answer}\"")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let content = format!(
            "User has answered your questions: {formatted}. You can now continue with the user's answers in mind."
        );
        Ok(Output::text(content, "Questions").meta(json!({ "answers": answers })))
    }
}
