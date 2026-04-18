# cf-mail-proxy

SMTP → Cloudflare Email Sending REST API 변환 프록시.

내부 서비스들이 표준 SMTP(포트 2525)로 메일을 보내면, 이 프록시가 CF Email Sending API(JSON POST)로 바꿔서 전송합니다.
SMTP 인증 불필요 — CF credential은 프록시 서버만 보관.

## 배포

```bash
make deploy     # 테스트 통과 후 LXC 50122에 자동 배포 + 재시작
make status     # 서비스 상태
make logs       # 실시간 로그
make test       # pytest 17개 케이스
```

## 구성

- `/usr/local/bin/cf-mail-proxy` — Python aiosmtpd 서버 (systemd 상주)
- `/etc/systemd/system/cf-mail-proxy.service` — unit
- `/etc/cf-mail-proxy.env` — CF 크리덴셜, DEFAULT_FROM, ALLOWED_DOMAINS

## 중앙관리

전 LXC postfix relayhost를 `[10.0.50.122]:2525`로 일괄 동기화:

```bash
pxi-mail-sync --dry-run   # 미리보기
pxi-mail-sync             # 적용
pxi-mail-sync --status    # 현황 조회
```

## 기능

- **MIME 인코딩 헤더 자동 디코딩**: `=?utf-8?Q?...?= 템플릿]` 혼합 형태도 정상 처리
- **FROM 도메인 rewrite**: CF 미등록 도메인(`root@openclaw.internal` 등)은 `DEFAULT_FROM`으로 자동 교체
- **HTML + plaintext 멀티파트 추출**: Laravel/MedusaJS 등이 보내는 AltBody 포함

## 환경변수

| 변수 | 설명 | 기본값 |
|---|---|---|
| `CF_ACCOUNT_ID` | Cloudflare account id | (필수) |
| `CLOUDFLARE_EMAIL` | CF 인증 이메일 | (필수) |
| `CLOUDFLARE_API_KEY` | CF Global API key | (필수) |
| `DEFAULT_FROM` | 미등록 도메인 rewrite 타겟 | `devops@prelik.com` |
| `ALLOWED_DOMAINS` | 쉼표구분 허용 도메인 | `prelik.com,ranode.net,internal.kr` |
| `PROXY_HOST` | 바인딩 IP | `0.0.0.0` |
| `PROXY_PORT` | SMTP 리슨 포트 | `2525` |

## 테스트

`test_proxy.py` — 17 케이스:
- 헤더 디코딩 (plain/B/Q/mixed/emoji/이름+주소)
- 본문 추출 (multipart/single/Korean)
- FROM 정책 (허용/차단/서브도메인)

CI: GitHub Actions `.github/workflows/ci.yml` — Python 3.11/3.12/3.13 매트릭스.

## 배경

2026-04-18 구축. Mailgun(100/일 무료) 한도 + 네이버 SPAM 판정으로 이탈.
CF Email Sending(3,000/월 무료, Workers $5 포함)로 전환.
