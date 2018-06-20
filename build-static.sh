#!/bin/bash
echo "Please make sure you have musl tools and kernel-headers-musl installed"
if [ ! -d temp/openssl ]; then
  echo "Building dependency OpenSSL"
  mkdir temp
  pushd temp
  git clone https://github.com/openssl/openssl
  PREFIX=$PWD/openssl-shared
  mkdir $PREFIX
  pushd openssl
  git checkout OpenSSL_1_1_0h
  CC="musl-gcc -fPIE -pie" LDFLAGS="-L/usr/lib" CFLAGS="-I/usr/lib/musl/include" ./Configure --prefix=$PREFIX linux-x86_64 no-shared no-async
  make && make install
  popd
  popd
fi
rm -rf target/release
rm -rf target/x86_64-unknown-linux-musl
OPENSSL_STATIC=1 OPENSSL_DIR=$PWD/temp/openssl-shared cargo build --release --target=x86_64-unknown-linux-musl