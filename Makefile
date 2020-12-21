# example .makerc:
# HOST=myhost.com
include .makerc

ifndef HOST
$(error HOST is not set in .makerc)
endif

ifndef PORT
PORT=22
endif

.PHONY: build-release
build-release:
	cargo build --release

.PHONY: deploy-latest
deploy-latest:
	./scripts/download_latest_release.sh
	scp -P $(PORT) /tmp/webcam-proxy $(HOST):webcam-proxy.tmp
	#rm /tmp/webcam-proxy
	ssh -t -p $(PORT) $(HOST) "sudo mv webcam-proxy.tmp /usr/local/bin/webcam-proxy/webcam-proxy && sudo chmod +x /usr/local/bin/webcam-proxy/webcam-proxy && sudo systemctl restart webcam-proxy.service"

.PHONY: run-nginx
run-nginx:
	docker run --rm -p 8080:80 --name my-custom-nginx-container -v $(pwd)/nginx.conf:/etc/nginx/nginx.conf:ro nginx
