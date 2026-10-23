#!/usr/bin/env python3
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        if self.path == "/v1/audio/voices":
            self.send_json(
                {
                    "data": [
                        {"id": "tara", "language": "en"},
                        {"id": "leah", "language": "en"},
                    ]
                }
            )
            return
        if self.path == "/server_info?config_format=json":
            self.send_json(
                {
                    "vllm_config": {
                        "model_config": {
                            "model": "org/vllm-only",
                            "max_model_len": 262144,
                        }
                    }
                }
            )
            return
        if self.path == "/v1/language-models":
            if self.headers.get("authorization") != "Bearer xai-e2e-secret":
                self.send_error(401)
                return
            self.send_json(
                {
                    "models": [
                        {
                            "id": "grok-4.3",
                            "aliases": ["grok-latest"],
                            "fingerprint": "fp_e2e",
                            "created": 3,
                            "owned_by": "xai",
                            "version": "1.0",
                            "input_modalities": ["text", "image"],
                            "output_modalities": ["text"],
                            "context_length": 131072,
                        }
                    ]
                }
            )
            return
        if self.path == "/models":
            self.send_json(
                {
                    "data": [
                        {
                            "id": "llama-alpha",
                            "status": {"value": "loaded"},
                            "architecture": {"input_modalities": ["text"]},
                        },
                        {
                            "id": "org/llama-beta",
                            "status": {"value": "sleeping"},
                            "architecture": {
                                "input_modalities": ["text", "image"]
                            },
                        },
                    ]
                }
            )
            return
        if self.path == "/api/v1/models":
            self.send_json(
                {
                    "models": [
                        {
                            "type": "llm",
                            "key": "alpha",
                            "display_name": "Alpha",
                            "loaded_instances": [{"id": "alpha-loaded"}],
                            "max_context_length": 32000,
                            "format": "gguf",
                            "capabilities": {"trained_for_tool_use": True},
                        },
                        {
                            "type": "llm",
                            "key": "compressor",
                            "display_name": "Dedicated Compressor",
                            "loaded_instances": [{"id": "compression-loaded"}],
                            "max_context_length": 16000,
                            "format": "gguf",
                            "capabilities": {"trained_for_tool_use": False},
                        },
                        {
                            "type": "llm",
                            "key": "org/beta",
                            "display_name": "Beta",
                            "loaded_instances": [],
                            "max_context_length": 64000,
                            "format": "mlx",
                            "capabilities": {
                                "vision": True,
                                "trained_for_tool_use": True,
                            },
                        },
                        {
                            "type": "llm",
                            "key": "slow",
                            "display_name": "Slow",
                            "loaded_instances": [{"id": "slow-loaded"}],
                            "max_context_length": 8000,
                            "format": "gguf",
                            "capabilities": {},
                        },
                    ]
                }
            )
            return
        if self.path != "/v1/models":
            self.send_error(404)
            return
        self.send_json(
            {
                "object": "list",
                "data": [
                    {"id": "alpha", "object": "model", "created": 1},
                    {"id": "org/beta", "object": "model", "created": 2},
                    {"id": "vllm-only", "object": "model", "created": 3},
                    {
                        "id": "ollama-cloud-model",
                        "object": "model",
                        "created": 3,
                        "owned_by": "ollama",
                    },
                ],
            }
        )

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        if self.path == "/v1/audio/transcriptions":
            content_type = self.headers.get("content-type", "")
            if not content_type.startswith("multipart/form-data;"):
                self.send_error(400, "multipart content type required")
                return
            if b'name="file"' not in body:
                self.send_error(400, "file field required")
                return
            if b'name="model"\r\n\r\nparakeet\r\n' not in body:
                self.send_error(400, "Infernaut did not rewrite the model field")
                return
            self.send_json({"text": "mock transcription"})
            return
        payload = json.loads(body)
        if payload.get("messages") == [{"role": "user", "content": "redirect-test"}]:
            self.send_response(307)
            self.send_header("Location", "http://127.0.0.1:18081/should-not-be-contacted")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.path in ("/v1/audio/speech", "/v1/audio/speech/stream"):
            if payload.get("model") != "tts-1":
                self.send_error(400, "Infernaut did not rewrite the TTS model")
                return
            if payload.get("voice") != "tara":
                self.send_error(400, "voice was not forwarded")
                return
            self.send_audio(stream=self.path.endswith("/stream"))
            return
        if payload.get("model") == "alpha" and self.headers.get(
            "authorization"
        ) != "Bearer openai-e2e-secret":
            self.send_error(401)
            return
        if payload.get("model", "").startswith("grok-") and self.headers.get(
            "authorization"
        ) != "Bearer xai-e2e-secret":
            self.send_error(401)
            return
        if payload.get("model") == "ollama-cloud-model" and self.headers.get(
            "authorization"
        ) != "Bearer ollama-e2e-secret":
            self.send_error(401)
            return
        if self.path == "/api/v1/models/load":
            model = payload["model"]
            self.send_json(
                {
                    "type": "llm",
                    "instance_id": f"{model.rsplit('/', 1)[-1]}-loaded",
                    "status": "loaded",
                }
            )
            return
        if self.path == "/models/load":
            self.send_json({"success": True})
            return
        if self.path == "/v1/messages":
            if self.headers.get("x-api-key") is not None:
                self.send_error(400, "Infernaut x-api-key leaked upstream")
                return
            if self.headers.get("anthropic-version") != "2023-06-01":
                self.send_error(400, "anthropic-version was not forwarded")
                return
            if payload.get("stream"):
                self.send_anthropic_sse(payload.get("model"))
                return
            self.send_json(
                {
                    "id": "msg_mock",
                    "type": "message",
                    "role": "assistant",
                    "model": payload.get("model"),
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "stop_sequence": None,
                    "usage": {"input_tokens": 5, "output_tokens": 1},
                }
            )
            return
        if payload.get("model") == "slow-loaded":
            time.sleep(2)
        if payload.get("stream"):
            self.send_sse(payload.get("model"))
            return
        if payload.get("tools"):
            self.send_json(
                {
                    "id": "mock-tool-response",
                    "object": "chat.completion",
                    "model": payload.get("model"),
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": None,
                                "tool_calls": [
                                    {
                                        "id": "call_probe",
                                        "type": "function",
                                        "function": {
                                            "name": "infernaut_probe",
                                            "arguments": '{"value":"ok"}',
                                        },
                                    }
                                ],
                            },
                            "finish_reason": "tool_calls",
                        }
                    ],
                }
            )
            return
        if payload.get("response_format"):
            self.send_json(
                {
                    "id": "mock-structured-response",
                    "object": "chat.completion",
                    "model": payload.get("model"),
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": '{"ok":true}',
                            },
                            "finish_reason": "stop",
                        }
                    ],
                }
            )
            return
        self.send_json(
            {
                "id": "mock-response",
                "object": "chat.completion",
                "model": payload.get("model"),
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop",
                    }
                ],
            }
        )

    def send_sse(self, model):
        chunks = (
            f'data: {json.dumps({"id": "mock-stream", "object": "chat.completion.chunk", "model": model, "choices": [{"index": 0, "delta": {"content": "OK"}}]})}\n\n'
            "data: [DONE]\n\n"
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(chunks)))
        self.end_headers()
        self.wfile.write(chunks)

    def send_anthropic_sse(self, model):
        message_start = json.dumps(
            {
                "type": "message_start",
                "message": {
                    "id": "msg_mock_stream",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": None,
                    "stop_sequence": None,
                    "usage": {"input_tokens": 5, "output_tokens": 0},
                },
            }
        )
        events = (
            'event: message_start\n'
            f'data: {message_start}\n\n'
            'event: content_block_start\n'
            'data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n'
            'event: content_block_delta\n'
            'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"OK"}}\n\n'
            'event: content_block_stop\n'
            'data: {"type":"content_block_stop","index":0}\n\n'
            'event: message_delta\n'
            'data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}\n\n'
            'event: message_stop\n'
            'data: {"type":"message_stop"}\n\n'
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(events)))
        self.end_headers()
        self.wfile.write(events)

    def send_audio(self, stream=False):
        encoded = b"RIFFMOCKWAVE"
        self.send_response(200)
        self.send_header("content-type", "audio/wav")
        self.send_header("content-length", str(len(encoded)))
        self.end_headers()
        if stream:
            self.wfile.write(encoded[:4])
            self.wfile.flush()
            time.sleep(0.01)
            self.wfile.write(encoded[4:])
        else:
            self.wfile.write(encoded)

    def send_json(self, payload):
        encoded = json.dumps(payload, separators=(",", ":")).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", 18080), Handler).serve_forever()
