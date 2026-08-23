# syntax=docker/dockerfile:1

FROM rust:bookworm

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        bison \
        build-essential \
        ca-certificates \
        clang \
        default-libmysqlclient-dev \
        default-mysql-client \
        default-mysql-server \
        flex \
        git \
        libclang-dev \
        libedit-dev \
        libjansson-dev \
        libpq-dev \
        libsqlite3-dev \
        libssl-dev \
        libxml2-dev \
        pkg-config \
        postgresql \
        postgresql-client \
        sqlite3 \
        uuid-dev \
    && rm -rf /var/lib/apt/lists/*

ARG ASTERISK_VERSION=22.7.0

RUN git clone --depth 1 --branch "${ASTERISK_VERSION}" \
        https://github.com/asterisk/asterisk.git /opt/asterisk-source \
    && cd /opt/asterisk-source \
    && ./configure \
        --prefix=/opt/asterisk-live \
        --sysconfdir=/opt/asterisk-live/etc \
        --localstatedir=/opt/asterisk-live/var \
        --without-pjproject-bundled \
        --without-jansson-bundled \
    && make menuselect.makeopts \
    && menuselect/menuselect \
        --enable res_config_sqlite3 \
        --enable res_config_pgsql \
        --enable res_config_mysql \
        menuselect.makeopts \
    && make -j2 \
    && make install \
    && test -f /opt/asterisk-live/lib/asterisk/modules/res_config_sqlite3.so \
    && test -f /opt/asterisk-live/lib/asterisk/modules/res_config_pgsql.so \
    && test -f /opt/asterisk-live/lib/asterisk/modules/res_config_mysql.so

WORKDIR /workspace
COPY . .

ENV ASTERISK_SOURCE_DIR=/opt/asterisk-source
ENV ASTERISK_BUILD_DIR=/opt/asterisk-source
ENV ASTERISK_VERSION=${ASTERISK_VERSION}
ENV RUSTFLAGS=-Dwarnings

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    cargo build --locked --release -p asterisk-module --no-default-features \
        --features asterisk-22 \
    && service postgresql start \
    && service mariadb start \
    && su postgres -c 'createuser root' \
    && su postgres -c 'createdb --owner=root sccp2_realtime' \
    && mysql --user=root --execute 'CREATE DATABASE sccp2_realtime CHARACTER SET utf8mb4' \
    && ASTERISK_BIN=/opt/asterisk-live/sbin/asterisk \
        ASTERISK_MODULE_DIR=/opt/asterisk-live/lib/asterisk/modules \
        ASTERISK_DATA_DIR=/opt/asterisk-live/var/lib/asterisk \
        ./asterisk-module/realtime/test-live-provider.sh \
            sqlite target/release/libchan_sccp2.so \
    && ASTERISK_BIN=/opt/asterisk-live/sbin/asterisk \
        ASTERISK_MODULE_DIR=/opt/asterisk-live/lib/asterisk/modules \
        ASTERISK_DATA_DIR=/opt/asterisk-live/var/lib/asterisk \
        ./asterisk-module/realtime/test-live-provider.sh \
            postgresql target/release/libchan_sccp2.so \
    && ASTERISK_BIN=/opt/asterisk-live/sbin/asterisk \
        ASTERISK_MODULE_DIR=/opt/asterisk-live/lib/asterisk/modules \
        ASTERISK_DATA_DIR=/opt/asterisk-live/var/lib/asterisk \
        ./asterisk-module/realtime/test-live-provider.sh \
            mysql target/release/libchan_sccp2.so
