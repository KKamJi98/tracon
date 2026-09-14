#!/bin/sh
# 공개 저장소에 회사 정보나 개인 경로가 들어가는 것을 막는다.
set -e
# 회사명 패턴은 일부러 넓게 잡아, 더 좁은 리터럴(사내 도메인, 메일 주소 등)을
# 통째로 포함한다. 겹친다고 해서 좁은 쪽을 지우거나 넓은 쪽을 좁히지 말 것 -
# 좁은 패턴은 문서화 목적이고, 실제 차단은 넓은 쪽이 한다.
PATTERNS='[Bb]unjang|[Bb]ungae|번개장터|TECH-[0-9]|DAC-[0-9]|/Users/ethan|ethan\.kim|@bunjang\.co\.kr|vault-company|vault-personal'
if git grep -nEI "$PATTERNS" -- . ':!scripts/scrub-check.sh' ; then
  echo "scrub-check: 공개 불가 문자열이 발견되었습니다"
  exit 1
fi
echo "scrub-check: ok"
