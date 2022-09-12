sudo perf record --call-graph dwarf -o /home/nikita/memoire/dpdk_picoquic/perfMeasures/perf_quiche_server.data taskset -c 3 target/release/examples/http3-server -p 4445 -k cert.key -c cert.crt
#sudo taskset -c 3 ./target/release/examples/http3-server -p 4445 -k cert.key -c cert.crt
