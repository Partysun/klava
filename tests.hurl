# Anthropic Proxy Server Tests

### Health Check
GET {{host}}/health

HTTP 200
[Asserts]
body == "OK"


### Basic Message Request
POST {{host}}/v1/messages
Content-Type: application/json

{
  "model": "gpt-4o",
  "max_tokens": 1024,
  "messages": [
    {
      "role": "user",
      "content": "Say hello"
    }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.type" == "message"
jsonpath "$.role" == "assistant"
jsonpath "$.content[0].type" == "text"
jsonpath "$.content[0].text" contains "Hello"
jsonpath "$.usage.input_tokens" isNumber
jsonpath "$.usage.output_tokens" isNumber


### Message with Temperature
POST {{host}}/v1/messages
Content-Type: application/json

{
  "model": "gpt-4o",
  "max_tokens": 256,
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
jsonpath "$.type" == "message"
jsonpath "$.content[0].text" contains "4"


### Streaming Request
POST {{host}}/v1/messages
Content-Type: application/json

{
  "model": "gpt-4o",
  "max_tokens": 512,
  "stream": true,
  "messages": [
    {
      "role": "user",
      "content": "Count to 3"
    }
  ]
}

HTTP 200
[Asserts]
header "Content-Type" == "text/event-stream"
header "Cache-Control" == "no-cache"
header "Connection" == "keep-alive"
body contains "event: message_start"
body contains "event: message_stop"


### System Message
POST {{host}}/v1/messages
Content-Type: application/json

{
  "model": "gpt-4o",
  "max_tokens": 256,
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
jsonpath "$.type" == "message"
jsonpath "$.content[0].type" == "text"
jsonpath "$.usage.input_tokens" isNumber
