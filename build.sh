cargo build --release
if [ $? -ne 0 ]; then
    echo "编译失败"
else
    cp target/release/qfclaw .
    ./qfclaw
fi
