#!/bin/sh
# 공개 저장소에 회사 정보나 개인 경로가 들어가는 것을 막는다.
set -e
PATTERNS='[Bb]unjang|[Bb]ungae|번개장터|TECH-[0-9]|DAC-[0-9]|/Users/ethan|ethan\.kim|@bunjang\.co\.kr|vault-company|vault-personal'
if git grep -nEI "$PATTERNS" -- . ':!scripts/scrub-check.sh' ; then
  echo "scrub-check: 공개 불가 문자열이 발견되었습니다"
  exit 1
fi
echo "scrub-check: ok"
