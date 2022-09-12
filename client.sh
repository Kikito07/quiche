sudo perf record --call-graph dwarf -o /home/nikita/memoire/dpdk_picoquic/perfMeasures/perf_quiche_client.data taskset -c 3 target/release/examples/http3-client -G 10000000000 -X keys.log 10.100.0.2 4445
#sudo taskset -c 3 ./target/release/examples/http3-client -G 5000000000 -X keys.log 10.100.0.2 4445
