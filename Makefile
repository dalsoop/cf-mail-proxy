.PHONY: test lint deploy status logs restart help

PROXY_VMID  ?= 50122
PROXY_BIN   ?= /usr/local/bin/cf-mail-proxy
PROXY_SVC   ?= cf-mail-proxy.service
PROXY_UNIT  ?= /etc/systemd/system/$(PROXY_SVC)
PROXY_ENV   ?= /etc/cf-mail-proxy.env

help:
	@awk 'BEGIN{FS=":.*##"; printf "targets:\n"} /^[a-zA-Z_-]+:.*##/{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

test: ## 단위 테스트 (pytest)
	python3 -m pytest test_proxy.py -v

lint: ## 문법·타입 점검 (py_compile)
	python3 -m py_compile proxy.py test_proxy.py

deploy: test ## 테스트 통과 후 LXC $(PROXY_VMID)에 배포
	@echo "=== proxy.py → LXC $(PROXY_VMID):$(PROXY_BIN) ==="
	pct push $(PROXY_VMID) proxy.py $(PROXY_BIN)
	pct exec $(PROXY_VMID) -- systemctl restart $(PROXY_SVC)
	@sleep 2
	pct exec $(PROXY_VMID) -- systemctl is-active $(PROXY_SVC)
	@echo "✓ 배포 완료"

status: ## 프록시 상태
	pct exec $(PROXY_VMID) -- systemctl status $(PROXY_SVC) --no-pager -n 5

logs: ## 실시간 로그
	pct exec $(PROXY_VMID) -- journalctl -u $(PROXY_SVC) -f

restart: ## 프록시 재시작
	pct exec $(PROXY_VMID) -- systemctl restart $(PROXY_SVC)

ci: lint test ## CI 전체 체크 (GitHub Actions와 동일)
