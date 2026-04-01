# OpenAI Proxy Server Tests

### Health Check
GET {{host}}/health

HTTP 200
[Asserts]
body == "OK"


### Basic Chat Completion Request
POST {{host}}/v1/chat/completions
Content-Type: application/json

{
  "model": "gpt-4o",
  "messages": [
    {
      "role": "user",
      "content": "Say hello"
    }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.object" == "chat.completion"
jsonpath "$.choices[0].message.role" == "assistant"
jsonpath "$.choices[0].message.content" contains "hello"
jsonpath "$.usage.prompt_tokens" isNumber
jsonpath "$.usage.completion_tokens" isNumber


### Chat Completion with Temperature
POST {{host}}/v1/chat/completions
Content-Type: application/json

{
  "model": "gpto",
  "temperature": 0.7,
  "messages": [
    {
      "role": "user",
      "content": "What is 2+2?"
    }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.object" == "chat.completion"
jsonpath "$.choices[0].message.content" contains "4"


### Streaming Chat Completion
POST {{host}}/v1/chat/completions
Content-Type: application/json

{
  "model": "gpt-4o",
  "stream": true,
  "messages": [
    {
      "role": "user",
      "content": "Count to"
    }
  ]
}

HTTP 200
[Asserts]
header "Content-Type" == "text/event-stream"
header "Cache-Control" == "no-cache"
header "Connection" == "keep-alive"
body contains "data: {"
body contains "[DONE]"


### System Message
POST {{host}}/v1/chat/completions
Content-Type: application/json

{
  "model": "gpt-4o",
  "system": "You are a helpful assistant that responds in one sentence.",
  "messages": [
    {
      "role": "user",
      "content": "Who are you?"
    }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.object" == "chat.completion"
jsonpath "$.choices[0].message.role" == "assistant"
jsonpath "$.choices[0].message.content" isString
jsonpath "$.usage.prompt_tokens" isNumber


### Multi-turn Conversation
POST {{host}}/v1/chat/completions
Content-Type: application/json

{
  "model": "gpt-4o",
  "messages": [
    {
      "role": "user",
      "content": "My name is Alice"
    },
    {
      "role": "assistant",
      "content": "Nice to meet you, Alice!"
    },
    {
      "role": "user",
      "content": "What's my name?"
    }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.choices[0].message.content" contains "Alice"
