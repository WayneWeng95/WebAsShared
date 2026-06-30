move from 4 nodes to 9 nodes cluster. 

Now we will run experiment with one node (node 0) as the coordinator + Redis host and the rest 8 for workers.

For wasmem its the only one will use all 9 nodes for compute, but the load on the host is capped at half (maxium 8 fan-outs)

the workers network should keep the 10.10.1.1 - 10.10.1.9 order, but we need to check on the node-0's ip.

node-0 remains on 10.10.1.2

cp /proj/sched-serv-PG0/.ssh/id_rsa ~/.ssh/id_rsa
chmod 600 ~/.ssh/id_rsa

