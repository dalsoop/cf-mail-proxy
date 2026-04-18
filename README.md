# cf-mail-proxy

SMTP → Cloudflare Email Sending REST API 변환 프록시 (Rust / tokio).

내부 서비스들이 표준 SMTP(포트 2525)로 메일을 보내면, 이 프록시가 CF Email Sending API(JSON POST)로 바꿔서 전송합니다.
SMTP 인증 불필요 — CF credential은 프록시 서버만 보관.

## 기술 스택

- **런타임**: tokio (single-thread, async)
- **파서**: mail-parser
- **HTTP 클라이언트**: reqwest + rustls
- **관찰성**: tracing
- **바이너리 크기**: ~4MB (strip + LTO)

## 배포

```bash
make deploy     # 테스트 통과 후 LXC 50122에 자동 배포 + 재시작
make status     # 서비스 상태
make logs       # 실시간 로그
make test       # cargo test (17 케이스)
make release    # 릴리즈 빌드만
```

## 구성

- `/usr/local/bin/cf-mail-proxy` — 정적 Rust 바이너리 (systemd 상주)
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

- **MIME 인코딩 헤더 자동 디코딩**: `=?utf-8?Q?...?= 템플릿]` 혼합 형태도 정상 처리 (RFC 2047 준수)
- **FROM 도메인 rewrite**: CF 미등록 도메인(`root@openclaw.internal` 등)은 `DEFAULT_FROM`으로 자동 교체
- **HTML + plaintext 멀티파트 추출**: Laravel/MedusaJS 등이 보내는 AltBody 포함
- **SMTP 준수**: EHLO, MAIL FROM, RCPT TO, DATA, RSET, NOOP, QUIT 지원. 리딩 닷 언스터핑 (RFC 5321)

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
| `RUST_LOG` | 로그 레벨 (trace/debug/info/warn/error) | `info` |

## 테스트

`src/main.rs` 내 `#[cfg(test)] mod tests` — 17 케이스:
- 헤더 디코딩 9: plain/empty/B/Q/mixed/emoji/이름+주소 등
- 주소 추출 3: MAIL FROM, RCPT TO, 홑꺾쇠 없는 형식
- FROM 정책 5: 허용/차단/서브도메인
- 멀티파트 파싱 1: Korean subject + text/html 추출

CI: GitHub Actions `.github/workflows/ci.yml` — fmt + clippy + test + release artifact.

## 버전 히스토리

- **0.2.0** (2026-04-18): Rust/tokio 재작성 (Python aiosmtpd에서 포팅)
- **0.1.0** (2026-04-18): Python aiosmtpd 초기 구현

## 배경

Mailgun(100/일 무료) 한도 + 네이버 SPAM 판정으로 이탈.
CF Email Sending(3,000/월 무료, Workers $5 포함)로 전환.
Rust로 재작성하여 정적 바이너리 단일 배포 + Python 런타임 제거.
