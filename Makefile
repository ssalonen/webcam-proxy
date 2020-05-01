# example .makerc:
# HOST=myhost.com
include .makerc

ifndef HOST
$(error HOST is not set in .makerc)
endif

.PHONY: build-release
build-release:
	cargo build --release

.PHONY: deploy
deploy: build-release
	scp target/release/webcam-proxy $(HOST):webcam-proxy.tmp
	ssh -t $(HOST) "sudo mv webcam-proxy.tmp /usr/local/bin/webcam-proxy/webcam-proxy && sudo systemctl restart webcam-proxy.service"


.PHONY: run-nginx
run-nginx:
	docker run --rm -p 8080:80 --name my-custom-nginx-container -v $(pwd)/nginx.conf:/etc/nginx/nginx.conf:ro nginx