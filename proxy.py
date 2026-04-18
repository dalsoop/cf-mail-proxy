#!/usr/bin/env python3
"""SMTP → Cloudflare Email Sending REST API 프록시.

Hi.events(Laravel)가 127.0.0.1:2525에 SMTP로 메일을 보내면,
이 프록시가 받아서 CF Email Sending API(JSON)로 변환해 POST한다.

필수 환경변수:
  CF_ACCOUNT_ID          Cloudflare account id
  CLOUDFLARE_EMAIL       CF 로그인 이메일 (X-Auth-Email)
  CLOUDFLARE_API_KEY     CF Global API key (X-Auth-Key)
"""

from __future__ import annotations

import asyncio
import email
import logging
import os
import sys
import base64 as _b64
import quopri as _qp
import re as _re
from email.message import Message


_ENCODED_WORD_RE = _re.compile(
    r"=\?([A-Za-z0-9_.\-]+)\?([BbQq])\?([^?\s]*?)\?=",
)


def _decode_encoded_word(match: "_re.Match[str]") -> str:
    charset, encoding, data = match.groups()
    if encoding.lower() == "b":
        try:
            raw = _b64.b64decode(data)
        except Exception:
            return match.group(0)
    else:  # Q
        raw = _qp.decodestring(data.replace("_", " "))
    try:
        return raw.decode(charset or "utf-8", errors="replace")
    except LookupError:
        return raw.decode("utf-8", errors="replace")


def header_to_str(value) -> str:
    """Email header (possibly MIME-encoded) → plain unicode str.

    Handles mixed encoded-word + plain parts (e.g. `=?utf-8?Q?...?= 템플릿]`).

    Python's stdlib ``email.header.decode_header`` mangles non-ASCII plain
    text mixed with encoded-words (str input gets backslash-u escaped).
    We avoid that by substituting only the encoded-word spans via regex.
    """
    if value is None:
        return ""
    raw = value if isinstance(value, str) else str(value)
    return _ENCODED_WORD_RE.sub(_decode_encoded_word, raw)

import requests
from aiosmtpd.controller import Controller
from aiosmtpd.smtp import Envelope, Session, SMTP

logging.basicConfig(
    level=os.environ.get("LOG_LEVEL", "INFO"),
    format="%(asctime)s %(levelname)s %(message)s",
)
log = logging.getLogger("cf-mail-proxy")


def required_env(name: str) -> str:
    v = os.environ.get(name)
    if not v:
        log.error("환경변수 %s 누락 — 종료", name)
        sys.exit(1)
    return v


CF_ACCOUNT = required_env("CF_ACCOUNT_ID")
CF_EMAIL = required_env("CLOUDFLARE_EMAIL")
CF_KEY = required_env("CLOUDFLARE_API_KEY")

# CF가 거부하는 non-verified FROM 도메인은 자동으로 이 주소로 rewrite
DEFAULT_FROM = os.environ.get("DEFAULT_FROM", "devops@prelik.com")
# 허용된 발신 도메인 (CF에 등록된 것). 비어있으면 모두 시도.
ALLOWED_DOMAINS = {
    d.strip().lower()
    for d in os.environ.get("ALLOWED_DOMAINS", "prelik.com,ranode.net,internal.kr").split(",")
    if d.strip()
}
CF_URL = (
    f"https://api.cloudflare.com/client/v4/accounts/{CF_ACCOUNT}/email/sending/send"
)
HEADERS = {
    "X-Auth-Email": CF_EMAIL,
    "X-Auth-Key": CF_KEY,
    "Content-Type": "application/json",
}


def decode_part(part: Message) -> str:
    payload = part.get_payload(decode=True) or b""
    charset = part.get_content_charset() or "utf-8"
    try:
        return payload.decode(charset)
    except (UnicodeDecodeError, LookupError):
        return payload.decode("utf-8", errors="replace")


def extract_bodies(msg: Message) -> tuple[str | None, str | None]:
    text = html = None
    if msg.is_multipart():
        for part in msg.walk():
            ctype = part.get_content_type()
            if part.is_multipart():
                continue
            if ctype == "text/plain" and text is None:
                text = decode_part(part)
            elif ctype == "text/html" and html is None:
                html = decode_part(part)
    else:
        body = decode_part(msg)
        if msg.get_content_type() == "text/html":
            html = body
        else:
            text = body
    return text, html


class CFHandler:
    async def handle_DATA(
        self, server: SMTP, session: Session, envelope: Envelope
    ) -> str:
        msg = email.message_from_bytes(envelope.content)

        from_hdr = header_to_str(msg.get("From")) or envelope.mail_from
        subject = header_to_str(msg.get("Subject")) or "(no subject)"
        reply_to = header_to_str(msg.get("Reply-To")) or None

        # Verified 도메인 아닌 경우 DEFAULT_FROM으로 rewrite
        import re as _re
        m = _re.search(r"<?([^<> ]+@[^<> ]+)>?", from_hdr)
        from_email = m.group(1) if m else from_hdr
        from_domain = from_email.split("@")[-1].lower() if "@" in from_email else ""
        if ALLOWED_DOMAINS and from_domain not in ALLOWED_DOMAINS:
            log.info("  FROM %s rewritten → %s (domain 허용 안 됨)", from_email, DEFAULT_FROM)
            from_hdr = DEFAULT_FROM

        text, html = extract_bodies(msg)

        results = []
        for rcpt in envelope.rcpt_tos:
            payload: dict = {
                "from": from_hdr,
                "to": rcpt,
                "subject": subject,
            }
            if html:
                payload["html"] = html
            if text:
                payload["text"] = text
            if reply_to:
                payload["reply_to"] = reply_to

            log.info("→ CF send  to=%s  subj=%r", rcpt, subject[:60])
            try:
                r = requests.post(CF_URL, json=payload, headers=HEADERS, timeout=20)
                data = r.json() if r.content else {}
            except Exception as e:
                log.error("CF API 호출 실패 to=%s: %s", rcpt, e)
                return f"451 Temporary CF API error: {e}"

            if r.ok and data.get("success"):
                delivered = data.get("result", {}).get("delivered", [])
                queued = data.get("result", {}).get("queued", [])
                bounced = data.get("result", {}).get("permanent_bounces", [])
                log.info(
                    "  ← delivered=%s queued=%s bounced=%s",
                    delivered,
                    queued,
                    bounced,
                )
                if bounced:
                    return f"550 Bounced by CF: {bounced}"
                results.append("ok")
            else:
                log.error("  ← CF rejected: %s", data)
                errs = data.get("errors") or data.get("error") or r.text[:200]
                return f"554 CF rejected: {errs}"

        return "250 Accepted via CF Email Sending"


def main() -> None:
    host = os.environ.get("PROXY_HOST", "0.0.0.0")
    port = int(os.environ.get("PROXY_PORT", "2525"))
    log.info("CF-mail-proxy 시작 %s:%d (CF account=%s)", host, port, CF_ACCOUNT)

    controller = Controller(CFHandler(), hostname=host, port=port)
    controller.start()
    try:
        asyncio.get_event_loop().run_forever()
    except KeyboardInterrupt:
        controller.stop()


if __name__ == "__main__":
    main()
