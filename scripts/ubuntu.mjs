$`docker run --network host ubuntu bash -c "sed -i 's/http:\\/\\/archive.ubuntu.com/http:\\/\\/localhost:9000/g' /etc/apt/sources.list && cat /etc/apt/sources.list && apt-get update"`