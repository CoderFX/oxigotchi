#!/bin/bash
# PiSugar double tap: toggle RAGE/SAFE mode
MODE=$(curl -s http://localhost:8080/api/status | grep -o '"mode":"[^"]*"' | grep -o ':[^}]*' | tr -d ':"')
if [ "$MODE" = "SAFE" ]; then
  curl -s -X POST http://localhost:8080/api/mode \
    -H 'Content-Type: application/json' \
    -d '{"mode":"RAGE"}' > /dev/null 2>&1
else
  curl -s -X POST http://localhost:8080/api/mode \
    -H 'Content-Type: application/json' \
    -d '{"mode":"SAFE"}' > /dev/null 2>&1
fi
