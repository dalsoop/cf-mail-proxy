"""cf-mail-proxy 단위 테스트.

실행:
    cd /opt/cf-mail-proxy && pytest -v
또는:
    make test
"""

from __future__ import annotations

import email
import os
import sys
import types
from email.message import EmailMessage

# Stub out deps before import so proxy.py's import-time ENV check / imports pass
os.environ.setdefault("CF_ACCOUNT_ID", "test-account")
os.environ.setdefault("CLOUDFLARE_EMAIL", "test@example.com")
os.environ.setdefault("CLOUDFLARE_API_KEY", "test-key")

# Stub aiosmtpd (not needed for unit tests of pure functions)
if "aiosmtpd" not in sys.modules:
    aiosmtpd = types.ModuleType("aiosmtpd")
    aiosmtpd.controller = types.ModuleType("aiosmtpd.controller")
    aiosmtpd.controller.Controller = object
    aiosmtpd.smtp = types.ModuleType("aiosmtpd.smtp")
    aiosmtpd.smtp.Envelope = object
    aiosmtpd.smtp.Session = object
    aiosmtpd.smtp.SMTP = object
    sys.modules["aiosmtpd"] = aiosmtpd
    sys.modules["aiosmtpd.controller"] = aiosmtpd.controller
    sys.modules["aiosmtpd.smtp"] = aiosmtpd.smtp

sys.path.insert(0, os.path.dirname(__file__))
import proxy as P  # noqa: E402


class TestHeaderDecoding:
    """msgid 제목·From 헤더의 MIME 인코딩 디코딩 검증."""

    def test_plain_ascii(self):
        assert P.header_to_str("Hello") == "Hello"

    def test_plain_korean(self):
        assert P.header_to_str("한국어 제목") == "한국어 제목"

    def test_none(self):
        assert P.header_to_str(None) == ""

    def test_utf8_b_encoded(self):
        # =?utf-8?B?...?= (Base64 encoded-word)
        encoded = "=?utf-8?B?7ZWc6rWt7Ja0IOygnOuqqQ==?="
        assert P.header_to_str(encoded) == "한국어 제목"

    def test_utf8_q_encoded(self):
        # =?utf-8?Q?...?= (Quoted-printable encoded-word)
        encoded = "=?utf-8?Q?=ED=95=9C=EA=B5=AD=EC=96=B4?="
        assert P.header_to_str(encoded) == "한국어"

    def test_mixed_encoded_and_plain(self):
        """실제 버그: Laravel이 보낸 =?utf-8?Q?=F0=9F=8E=9F?= 템플릿] 형식."""
        raw = "=?utf-8?Q?=F0=9F=8E=9F=EF=B8=8F_=5B=EA=B0=95=EC=9D=98=EC=8B=A4_?= 템플릿]"
        out = P.header_to_str(raw)
        assert "🎟️" in out
        assert "[강의실" in out
        assert "템플릿]" in out
        assert "=?" not in out  # 인코딩 토큰이 남아있으면 안 됨

    def test_emoji_only(self):
        encoded = "=?utf-8?B?8J+Onw==?="
        assert P.header_to_str(encoded) == "🎟"

    def test_with_email_address(self):
        """From 헤더 형식: "이름" <email@domain>"""
        encoded = '=?utf-8?B?7ZWY7Jik?= <noreply@prelik.com>'
        out = P.header_to_str(encoded)
        assert "하오" in out
        assert "<noreply@prelik.com>" in out


class TestBodyExtraction:
    """multipart/alternative 메시지에서 text/html 추출."""

    def _make_multipart(self, text_body: str, html_body: str) -> email.message.Message:
        msg = EmailMessage()
        msg["Subject"] = "x"
        msg["From"] = "a@b.com"
        msg["To"] = "c@d.com"
        msg.set_content(text_body)
        msg.add_alternative(html_body, subtype="html")
        return msg

    def test_multipart_extracts_both(self):
        msg = self._make_multipart("plain body", "<p>html body</p>")
        text, html = P.extract_bodies(msg)
        assert text and "plain body" in text
        assert html and "html body" in html

    def test_multipart_korean(self):
        msg = self._make_multipart("안녕하세요", "<p>안녕하세요</p>")
        text, html = P.extract_bodies(msg)
        assert "안녕하세요" in text
        assert "안녕하세요" in html

    def test_single_part_text(self):
        msg = EmailMessage()
        msg.set_content("plain only")
        text, html = P.extract_bodies(msg)
        assert text and "plain only" in text
        assert html is None

    def test_single_part_html(self):
        msg = EmailMessage()
        msg.set_content("<h1>html only</h1>", subtype="html")
        text, html = P.extract_bodies(msg)
        assert html and "html only" in html


class TestFromDomainPolicy:
    """ALLOWED_DOMAINS + DEFAULT_FROM 리라이트 로직 (프록시가 외부로 보낼 FROM 선택)."""

    def setup_method(self):
        self._prev_allowed = P.ALLOWED_DOMAINS.copy()
        self._prev_default = P.DEFAULT_FROM
        P.ALLOWED_DOMAINS.clear()
        P.ALLOWED_DOMAINS.update({"prelik.com", "ranode.net", "internal.kr"})
        P.DEFAULT_FROM = "devops@prelik.com"

    def teardown_method(self):
        P.ALLOWED_DOMAINS.clear()
        P.ALLOWED_DOMAINS.update(self._prev_allowed)
        P.DEFAULT_FROM = self._prev_default

    def _rewrite(self, from_hdr: str) -> str:
        """_handler의 rewrite 로직을 재현 (현재는 private inline; 향후 함수화 대상)."""
        import re as _re

        m = _re.search(r"<?([^<> ]+@[^<> ]+)>?", from_hdr)
        from_email = m.group(1) if m else from_hdr
        from_domain = (
            from_email.split("@")[-1].lower() if "@" in from_email else ""
        )
        if P.ALLOWED_DOMAINS and from_domain not in P.ALLOWED_DOMAINS:
            return P.DEFAULT_FROM
        return from_hdr

    def test_allowed_passes(self):
        assert self._rewrite("devops@prelik.com") == "devops@prelik.com"

    def test_allowed_with_name_passes(self):
        assert (
            self._rewrite('"Hi.Events" <noreply@prelik.com>')
            == '"Hi.Events" <noreply@prelik.com>'
        )

    def test_blocked_rewritten(self):
        assert self._rewrite("root@openclaw.internal") == "devops@prelik.com"

    def test_internal_kr_allowed(self):
        assert self._rewrite("devops@internal.kr") == "devops@internal.kr"

    def test_subdomain_blocked(self):
        """prelik.com 만 허용 — 서브도메인은 기본적으로 차단."""
        assert (
            self._rewrite("user@mail.prelik.com") == "devops@prelik.com"
        )
