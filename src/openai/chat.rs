use crate::chat::{ChatOptions,ChatResult,ChatMessage,ChatMessages,ChatRole,ChatError};
use std::io::{self,Write};
use std::env;
use async_recursion::async_recursion;
use serde::{Serialize,Deserialize};
use reqwest::{Client,RequestBuilder};
use reqwest_eventsource::{EventSource,Event};
use serde_json::json;
use futures_util::stream::StreamExt;
use crate::openai::response::OpenAICompletionResponse;
use crate::Config;

pub struct OpenAIChatCommand {
    options: ChatOptions
}

impl TryFrom<ChatOptions> for OpenAIChatCommand {
    type Error = ChatError;

    fn try_from(options: ChatOptions) -> Result<Self, Self::Error> {
        Ok(OpenAIChatCommand { options })
    }
}

impl OpenAIChatCommand {
    #[async_recursion]
    pub async fn run(&mut self, client: &Client, config: &Config) -> ChatResult {
        let options = &mut self.options;
        let print_output = !options.completion.quiet.unwrap_or(false);

        loop {
            if options.stream {
                let result = handle_stream(client, options, config).await?;
                if result.len() > 0 {
                    return Ok(result);
                }
            } else {
                let result = handle_sync(client, options, config, print_output).await?;
                if result.len() > 0 {
                    return Ok(result);
                }
            }

            if let None = options.file.read(None, Some(&*options.prefix_user), options.no_context) {
                return Ok(vec![]);
            }
        }
    }
}

async fn handle_sync(client: &Client, options: &mut ChatOptions, config: &Config, print_output: bool) -> ChatResult {
    let request = get_request(&client, &options, &config, false)?
        .send()
        .await
        .expect("Failed to send chat");

    if !request.status().is_success() {
        return Err(ChatError::OpenAIError(request.json().await?));
    }

    let chat_response: OpenAICompletionResponse<OpenAIChatChoice> = request.json().await?;
    let text = chat_response.choices.first().unwrap().message
        .as_ref()
        .map(|message| {
            let message = message.content.trim();

            if message.to_lowercase().starts_with(&options.prefix_ai) {
                message.to_string()
            } else {
                format!("{}: {}", options.prefix_ai, message)
            }
        });

    if let Some(text) = text {
        let text = options.file.write(text, options.no_context, false)?;

        if print_output {
            println!("{}", text);
        }

        if options.completion.append.is_some() || options.completion.once.unwrap_or(false) {
            return Ok(ChatMessages::try_from(&*options)?);
        }
    }

    Ok(vec![])
}

async fn handle_stream(client: &Client, options: &mut ChatOptions, config: &Config) -> ChatResult {
    let post = get_request(client, options, config, true)?;
    let mut stream = EventSource::new(post).unwrap();
    let mut state = StreamMessageState::New;
    let mut response = String::new();

    'stream: while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Open) => {},
            Ok(Event::Message(message)) if message.data == "[DONE]" => {
                break 'stream;
            },
            Ok(Event::Message(message)) => {
                state = handle_stream_message(options, message.data, &mut response, state)?;
            },
            Err(err) => {
                stream.close();
                return Err(ChatError::EventSource(err));
            }
        }
    }

    match state {
        StreamMessageState::New => {},
        StreamMessageState::HasWrittenRole |
        StreamMessageState::HasWrittenContent => {
            println!("");
            response += "\n";
            io::stdout().flush().unwrap();
        },
    }

    options.file.write(response, options.no_context, false);

    if options.completion.append.is_some() || options.completion.once.unwrap_or(false) {
        return Ok(ChatMessages::try_from(&*options)?);
    }

    Ok(vec![])
}

fn get_request(client: &Client, options: &ChatOptions, config: &Config, stream: bool) -> Result<RequestBuilder, ChatError> {
    let messages = ChatMessages::try_from(options)?;

    Ok(client.post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(env::var("OPEN_AI_API_KEY")
            .ok()
            .or_else(|| config.api_key_openai.clone())
            .ok_or_else(|| ChatError::Unauthorized)?
        )
        .json(&json!({
            "model": "gpt-4",
            "temperature": options.temperature,
            "messages": messages,
            "stream": stream
        }))
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum StreamMessageState {
    New,
    HasWrittenRole,
    HasWrittenContent,
}

fn handle_stream_message(
    options: &mut ChatOptions,
    message: String,
    response: &mut String,
    mut state: StreamMessageState) -> Result<StreamMessageState, ChatError>
{
    let chat_response: OpenAICompletionResponse<OpenAIChatDelta> =
        serde_json::from_str(&message)?;

    let delta = &chat_response.choices.first().unwrap().delta;
    if let Some(ref role) = delta.role {
        print!("{}", role);
        response.push_str(&format!("{role}"));
        state = StreamMessageState::HasWrittenRole;
    }
    if let Some(content) = delta.content.clone() {
        let filtered = match state {
            StreamMessageState::New |
            StreamMessageState::HasWrittenRole => {
                let filtered = content.trim_start();
                let prefix_ai = &format!("{}:", options.prefix_ai);

                if filtered.starts_with(prefix_ai) {
                    filtered
                        .replacen(prefix_ai, "", 1)
                        .trim_start()
                        .to_string()
                } else {
                    filtered.to_string()
                }
            },
            StreamMessageState::HasWrittenContent => content,
        };

        print!("{}", filtered);
        state = StreamMessageState::HasWrittenContent;
        response.push_str(&filtered);
    }
    io::stdout().flush().unwrap();
    Ok(state)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenAIChatChoice {
    index: Option<usize>,
    message: Option<ChatMessage>,
    finish_reason: Option<OpenAIFinishReason>
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAIFinishReason {
    Stop,
    Length,
    ContentFilter
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenAIChatDelta {
    index: Option<usize>,
    delta: ChatMessageDelta,
    finish_reason: Option<String>
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChatMessageDelta {
    pub role: Option<ChatRole>,
    pub content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::*;
    use crate::completion::*;

    #[test]
    fn transcript_with_multiple_lines() {
        let system = String::from("You're a duck. Say quack.");
        let file = CompletionFile {
            file: None,
            overrides: ChatCommand::default(),
            transcript: concat!(
                "USER: hey\n",
                concat!(
                    "AI: I'm a multimodel super AI hell bent on destroying the world.\n",
                    "How can I help you today?"
                )
            ).to_string()
        };
        let options = ChatOptions {
            system: system.clone(),
            file,
            tokens_max: 4096,
            tokens_balance: 0.5,
            ..ChatOptions::default()
        };
        assert_eq!(ChatMessages::try_from(&options).unwrap(), vec![
            ChatMessage::new(ChatRole::System, system),
            ChatMessage::new(ChatRole::User, "hey"),
            ChatMessage::new(ChatRole::Ai, concat!(
                "I'm a multimodel super AI hell bent on destroying the world.\n",
                "How can I help you today?"
            )),
        ]);
    }

    #[test]
    fn transcript_handles_labels_correctly() {
        let system = String::from("You're a duck. Say quack.");
        let file = CompletionFile {
            file: None,
            overrides: ChatCommand::default(),
            transcript: concat!(
                "USER: hey\n",
                concat!(
                    "AI: I'm a multimodel super AI hell bent on destroying the world.\n",
                    "For example: This might have screwed up before"
                )
            ).to_string()
        };
        let options = ChatOptions {
            tokens_max: 4000,
            tokens_balance: 0.5,
            system: system.clone(),
            file,
            ..ChatOptions::default()
        };
        assert_eq!(ChatMessages::try_from(&options).unwrap(), vec![
            ChatMessage::new(ChatRole::System, system),
            ChatMessage::new(ChatRole::User, "hey"),
            ChatMessage::new(ChatRole::Ai, concat!(
                "I'm a multimodel super AI hell bent on destroying the world.\n",
                "For example: This might have screwed up before"
            )),
        ]);
    }

    #[test]
    fn transcript_labotomizes_itself() {
        let system = String::from("You're a duck. Say quack.");
        let file = CompletionFile {
            file: None,
            overrides: ChatCommand::default(),
            transcript: concat!(
                "USER: hey. This is a really long message to ensure that it gets labotomized.\n",
                "AI: hey"
            ).to_string()
        };
        let options = ChatOptions {
            tokens_max: 40,
            tokens_balance: 0.5,
            system: system.clone(),
            file,
            ..ChatOptions::default()
        };
        assert_eq!(ChatMessages::try_from(&options).unwrap(), vec![
            ChatMessage::new(ChatRole::System, system),
            ChatMessage::new(ChatRole::Ai, "hey"),
        ]);
    }

    #[test]
    fn streaming_strips_whitespace_and_labels_from_delta_content() {
        let file = CompletionFile {
            file: None,
            overrides: ChatCommand::default(),
            transcript: String::new()
        };
        let mut options = ChatOptions {
            tokens_max: 40,
            tokens_balance: 0.5,
            prefix_ai: "AI".into(),
            file,
            ..ChatOptions::default()
        };
        let chat_response = String::from(r#"{
            "choices": [
                {
                    "delta": {
                        "role": "assistant",
                        "content": "\n     AI: hey there"
                    }
                }
            ],
            "created": 0,
            "model": "",
            "object": "",
            "id": ""
        }"#);

        let state = handle_stream_message(&mut options, chat_response, StreamMessageState::New)
            .unwrap();

        assert_eq!(StreamMessageState::HasWrittenContent, state);
        assert_eq!("AI: hey there", &options.file.transcript)
    }
}
