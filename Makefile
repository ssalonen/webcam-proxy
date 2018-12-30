
.PHONY: run-nginx



run-nginx:
	docker run --rm -p 8080:80 --name my-custom-nginx-container -v $(pwd)/nginx.conf:/etc/nginx/nginx.conf:ro nginx