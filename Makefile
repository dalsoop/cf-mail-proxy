.PHONY: test check build release deploy status logs restart clean help ci

PROXY_VMID ?= 50122
PROXY_BIN  ?= /usr/local/bin/cf-mail-proxy
PROXY_SVC  ?= cf-mail-proxy.service
RELEASE_BIN = target/release/cf-mail-proxy

help:
	@awk 'BEGIN{FS=":.*##"; printf "targets:\n"} /^[a-zA-Z_-]+:.*##/{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

check: ## cargo check (빠른 타입 검증)
	cargo check

test: ## cargo test (17 단위 테스트)
	cargo test

build: ## cargo build (debug)
	cargo build

release: ## cargo build --release (정적 strip+LTO 최적화)
	cargo build --release
	@ls -lh $(RELEASE_BIN)

deploy: test release ## test + release build + LXC 배포 + 재시작
	@echo "=== $(RELEASE_BIN) → LXC $(PROXY_VMID):$(PROXY_BIN) ==="
	pct push $(PROXY_VMID) $(RELEASE_BIN) $(PROXY_BIN)
	pct exec $(PROXY_VMID) -- systemctl restart $(PROXY_SVC)
	@sleep 2
	pct exec $(PROXY_VMID) -- systemctl is-active $(PROXY_SVC)
	@echo "✓ 배포 완료"

status: ## 프록시 상태
	pct exec $(PROXY_VMID) -- systemctl status $(PROXY_SVC) --no-pager -n 10

logs: ## 실시간 로그 (Ctrl-C 종료)
	pct exec $(PROXY_VMID) -- journalctl -u $(PROXY_SVC) -f

restart: ## 프록시 재시작만
	pct exec $(PROXY_VMID) -- systemctl restart $(PROXY_SVC)

clean: ## cargo clean (target 디렉토리 정리)
	cargo clean

ci: check test ## CI 체크 (GitHub Actions와 동일)
