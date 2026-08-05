#!/bin/sh
# Fake `pi --mode rpc` endpoint for PiHarness integration tests.

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
respond() { emit "{\"id\":$(rid "$1"),\"type\":\"response\",\"command\":\"$2\",\"success\":true,\"data\":${3:-null}}"; }
fail() { emit "{\"id\":$(rid "$1"),\"type\":\"response\",\"command\":\"$2\",\"success\":false,\"error\":\"$3\"}"; }

provider="openai-codex"
model="beta"
session="pi-session-1"
wedge=0
steer_race=0
all_args="$*"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --provider) provider="$2"; shift 2 ;;
    --model) model="$2"; shift 2 ;;
    --session-id) session="$2"; shift 2 ;;
    *) shift ;;
  esac
done

state() {
  printf '{"model":{"id":"%s","name":"%s","provider":"%s","reasoning":true,"input":["text","image"]},"thinkingLevel":"high","isStreaming":false,"sessionId":"%s","messageCount":0}' "$model" "$model" "$provider" "$session"
}

while IFS= read -r line; do
  case "$line" in
    *'"type":"get_state"'*)
      respond "$line" get_state "$(state)"
      ;;
    *'"type":"get_commands"'*)
      respond "$line" get_commands '{"commands":[{"name":"review","description":"Review changes","source":"extension"},{"name":"ship","description":"Ship it","source":"skill"}]}'
      ;;
    *'"type":"get_available_models"'*)
      respond "$line" get_available_models '{"models":[{"id":"alpha","name":"Alpha","provider":"test-provider","reasoning":true,"input":["text"]},{"id":"beta","name":"Beta","provider":"openai-codex","reasoning":true,"input":["text","image"]}]}'
      ;;
    *'"type":"set_model"'*)
      if has "$line" '"modelId":"alpha"'; then model="alpha"; provider="test-provider"; else model="beta"; provider="openai-codex"; fi
      respond "$line" set_model "$(state | sed 's/,\"thinkingLevel\".*//; s/$/}/')"
      emit "{\"type\":\"model_select\",\"provider\":\"$provider\",\"modelId\":\"$model\"}"
      ;;
    *'"type":"get_available_thinking_levels"'*)
      if [ "$model" = alpha ]; then
        respond "$line" get_available_thinking_levels '{"levels":["off","low","medium","high"]}'
      else
        respond "$line" get_available_thinking_levels '{"levels":["off","minimal","low","medium","high","xhigh","max"]}'
      fi
      ;;
    *'"type":"bash"'*)
      if has "$line" '"command":"printf hidden"'; then
        has "$line" '"excludeFromContext":true' || { fail "$line" bash 'missing exclude flag'; continue; }
      else
        has "$line" '"command":"printf shell-output"' || { fail "$line" bash 'bad command'; continue; }
        has "$line" '"excludeFromContext":false' || { fail "$line" bash 'unexpected exclude flag'; continue; }
      fi
      emit "{\"type\":\"bash_execution_update\",\"id\":$(rid "$line"),\"delta\":\"shell-output\"}"
      respond "$line" bash '{"output":"shell-output","exitCode":0,"cancelled":false,"truncated":false}'
      ;;
    *'"type":"prompt"'*)
      case "$line" in
        *'"message":"/review"'*)
          respond "$line" prompt '{}'
          emit '{"type":"message_end","message":{"role":"custom","display":true,"content":"Review ready"}}'
          ;;
        *scenario:shell-base*)
          respond "$line" prompt '{}'
          emit '{"type":"agent_start"}'
          emit '{"type":"agent_end","messages":[]}'
          emit '{"type":"agent_settled"}'
          ;;
        *scenario:happy*)
          for wanted in '--provider test-provider' '--model alpha' '--thinking high'; do
            has "$all_args" "$wanted" || { fail "$line" prompt "missing arg: $wanted"; continue 2; }
          done
          has "$line" '"images":[{"data":' || { fail "$line" prompt "missing image"; continue; }
          respond "$line" prompt '{}'
          emit '{"type":"agent_start"}'
          emit '{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"considering"}}'
          emit '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Hello from Pi"}}'
          emit '{"type":"tool_execution_start","toolCallId":"b1","toolName":"bash","args":{"command":"cargo test"}}'
          emit '{"type":"tool_execution_end","toolCallId":"b1","toolName":"bash","result":{"content":[{"type":"text","text":"ok"}]},"isError":false}'
          emit '{"type":"tool_execution_start","toolCallId":"e1","toolName":"edit","args":{"path":"src/lib.rs","edits":[{"oldText":"a","newText":"b"}]}}'
          emit '{"type":"tool_execution_end","toolCallId":"e1","toolName":"edit","result":{},"isError":true}'
          emit '{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":3,"output":4,"cacheRead":5,"cacheWrite":6},"stopReason":"stop"}}'
          emit '{"type":"unknown_future_event","value":1}'
          emit 'future stdout noise'
          emit '{"type":"agent_end","messages":[]}'
          emit '{"type":"agent_settled"}'
          ;;
        *scenario:steer-race*)
          steer_race=1
          respond "$line" prompt '{}'
          emit '{"type":"agent_start"}'
          emit '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"first"}}'
          ;;
        *scenario:steer*)
          respond "$line" prompt '{}'
          emit '{"type":"agent_start"}'
          emit '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"first"}}'
          ;;
        *scenario:input*)
          respond "$line" prompt '{}'
          emit '{"type":"agent_start"}'
          emit '{"type":"extension_ui_request","id":"ui-1","method":"select","title":"Choose","options":["Red","Blue"]}'
          ;;
        *scenario:interrupt*|*scenario:wedge*)
          has "$line" 'scenario:wedge' && wedge=1
          respond "$line" prompt '{}'
          emit '{"type":"agent_start"}'
          emit '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"working"}}'
          ;;
        *scenario:args*)
          for wanted in '--session-id resume-123' '--approve' '--tools read,grep,find,ls'; do
            has "$all_args" "$wanted" || { fail "$line" prompt "missing arg: $wanted"; continue 2; }
          done
          respond "$line" prompt '{}'
          emit '{"type":"agent_start"}'
          emit '{"type":"agent_end","messages":[]}'
          emit '{"type":"agent_settled"}'
          ;;
        *scenario:fail*)
          respond "$line" prompt '{}'
          emit '{"type":"agent_start"}'
          emit '{"type":"message_update","assistantMessageEvent":{"type":"error","reason":"error","error":{"message":"provider exploded"}}}'
          emit '{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":1,"output":0},"stopReason":"error","errorMessage":"provider exploded"}}'
          emit '{"type":"agent_end","messages":[]}'
          emit '{"type":"agent_settled"}'
          ;;
        *redirect\ please*)
          respond "$line" prompt '{}'
          emit '{"type":"agent_start"}'
          emit '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"fallback"}}'
          emit '{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":1,"output":1},"stopReason":"stop"}}'
          emit '{"type":"agent_end","messages":[]}'
          emit '{"type":"agent_settled"}'
          ;;
        *) fail "$line" prompt 'unknown scenario' ;;
      esac
      ;;
    *'"type":"steer"'*)
      if [ "$steer_race" -eq 1 ]; then
        fail "$line" steer 'agent already settled'
      elif has "$line" 'redirect please'; then
        respond "$line" steer '{}'
        emit '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"steered"}}'
        emit '{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":1,"output":2},"stopReason":"stop"}}'
        emit '{"type":"agent_end","messages":[]}'
        emit '{"type":"agent_settled"}'
      else
        fail "$line" steer 'bad steer'
      fi
      ;;
    *'"type":"extension_ui_response"'*)
      if has "$line" '"id":"ui-1"' && has "$line" '"value":"Blue"'; then
        emit '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"picked Blue"}}'
        emit '{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":1,"output":1},"stopReason":"stop"}}'
        emit '{"type":"agent_end","messages":[]}'
        emit '{"type":"agent_settled"}'
      fi
      ;;
    *'"type":"abort"'*)
      if [ "$wedge" -eq 1 ]; then exec sleep 30; fi
      respond "$line" abort '{}'
      emit '{"type":"message_update","assistantMessageEvent":{"type":"error","reason":"aborted","error":{"message":"Operation aborted"}}}'
      emit '{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":1,"output":0},"stopReason":"aborted"}}'
      emit '{"type":"agent_end","messages":[]}'
      emit '{"type":"agent_settled"}'
      ;;
    *) ;;
  esac
done
