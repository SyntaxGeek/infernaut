# Hermes STT and TTS through Infernaut

Hermes' OpenAI audio integrations can use the same Infernaut base URL and a
dedicated named-client key. With `model_id_format = "colon_qualified"`, use the
published audio route IDs rather than the physical upstream model names:

```bash
INFERNAUT_AUDIO_BASE_URL="http://127.0.0.1:4040/v1"
STT_MODEL="route:private:transcription"
TTS_MODEL="route:private:speech"
TTS_VOICE="tara"

# Supply the same secret configured by the Infernaut client's
# api_key_env = "INFERNAUT_HERMES_KEY" entry.
: "${INFERNAUT_HERMES_KEY:?export the Hermes named-client key first}"

# Uppercase keys are persisted by Hermes in $HERMES_HOME/.env. Using the
# dedicated voice key makes the OpenAI SDK send it as a Bearer token to
# Infernaut for both STT and TTS.
hermes config set VOICE_TOOLS_OPENAI_KEY "$INFERNAUT_HERMES_KEY"
hermes config set STT_OPENAI_BASE_URL "$INFERNAUT_AUDIO_BASE_URL"

hermes config set stt.enabled true
hermes config set stt.provider openai
hermes config set stt.language en
hermes config set stt.echo_transcripts true
hermes config set stt.openai.base_url "$INFERNAUT_AUDIO_BASE_URL"
hermes config set stt.openai.model "$STT_MODEL"

hermes config set tts.provider openai
hermes config set tts.openai.base_url "$INFERNAUT_AUDIO_BASE_URL"
hermes config set tts.openai.model "$TTS_MODEL"
hermes config set tts.openai.voice "$TTS_VOICE"
hermes config set tts.speed 1.0

hermes tools enable tts
hermes config check

# Reload the gateway once no Hermes turn is actively running.
hermes gateway restart
```

The duplicated STT base URL in `.env` and `config.yaml` is intentional for
compatibility across Hermes releases; current versions prefer the explicit
`stt.openai.base_url` configuration. Hermes' OpenAI TTS backend calls the
standard `/v1/audio/speech` endpoint and consumes its response incrementally;
its higher-level streaming mode synthesizes sentence by sentence. Infernaut's
explicit `/v1/audio/speech/stream` endpoint remains available to clients that
request that provider extension directly.

Audio requests use the same Infernaut authentication and privacy policy as
chat, but they do not enter the prompt classifier. Request audio is excluded
from plugin content events and core logs; telemetry records only routing
metadata and total request size. Static audio models and routes stay out of
the chat picker and adaptive LLM pools. The legacy `openai_transcription`
kind remains available for STT-only servers.

See the [API surface](../README.md#api-surface) for the audio endpoints and
[Guided configuration](CONFIGURATION.md) for defining dedicated transcription
and speech routes.
