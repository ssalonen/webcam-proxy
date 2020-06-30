cat << EOF > $1/Dockerfile
FROM rustembedded/cross:$1-$2

RUN apt-get update && apt-get install -y libssl-dev
EOF