#!/bin/bash
# PiSugar single tap: cycle rage level 1-6
RATE=$(curl -s http://localhost:8080/api/status | grep -o '"attack_rate":[0-9]*' | grep -o '[0-9]*')
NEXT=$(( (${RATE:-1} % 6) + 1 ))
curl -s -X POST http://localhost:8080/api/rate \
  -H 'Content-Type: application/json' \
  -d "{\"rate\":$NEXT}" > /dev/null 2>&1
