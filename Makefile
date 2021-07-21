build:
	cargo build

run:
	cargo run

test:
	docker run --name redis_test -d -p 3001:6379 --rm redis
	cargo test; docker stop redis_test

dev:
	cargo watch -d 2 -i cache -x run

dev_test:
	cargo watch -d 1 -i cache -s "docker run --name redis_test -d -p 3001:6379 --rm redis && cargo test; docker stop redis_test"

dev_clear:
	rm -rf cache/* 
	docker stop redis_dev || return 0
	docker run --name redis_dev -d --network host --rm redis
	cargo watch -d 2 -i cache -x run

dev_deps:
	cargo install cargo-watch
	yarn global add zx

redis:
	docker run --name redis_dev -d --network host --rm redis

redis_stop:
	docker stop redis_dev

redis_test:
	docker run --name redis_test -d -p 3001:6379 --rm redis

redis_cli:
	docker run -it --network host --rm redis redis-cli

test_redis_cli:
	docker run -it --network host --rm redis redis-cli -p 3001

redis_dump:
	docker run -it --network host --rm redis redis-cli get total_size
	docker run -it --network host --rm redis redis-cli zrange cache_keys 0 -1 WITHSCORES

test_redis_dump:
	docker run -it --network host --rm redis redis-cli -p 3001 get total_size
	docker run -it --network host --rm redis redis-cli -p 3001 zrange cache_keys 0 -1 WITHSCORES

clean:
	rm -rf cache/* 
	docker stop redis_dev || return 0
	docker stop redis_test || return 0
