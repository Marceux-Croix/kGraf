import math

import networkx as nx

# Fixed multigraph matching the Rust test fixture `matches_networkx_reference`.
G = nx.MultiDiGraph()
edges = [
    ("a.rs", "b.rs", 10.0 * math.sqrt(3)),
    ("a.rs", "c.rs", 1.0),
    ("a.rs", "c.rs", 2.0),  # a parallel edge: networkx sums them
    ("b.rs", "c.rs", 1.0 * math.sqrt(2)),
    ("c.rs", "a.rs", 0.1),
    ("d.rs", "c.rs", 100.0),
    ("d.rs", "d.rs", 0.1),
    ("e.rs", "e.rs", 0.1),
]
for s, d, w in edges:
    G.add_edge(s, d, weight=w)
r = nx.pagerank(G, weight="weight")
print("plain", [(k, repr(v)) for k, v in sorted(r.items())])
pers = {"a.rs": 50.0, "e.rs": 50.0}
r = nx.pagerank(G, weight="weight", personalization=pers, dangling=pers)
print("pers", [(k, repr(v)) for k, v in sorted(r.items())])
G.add_edge("e.rs", "f.rs", weight=2.0)
r = nx.pagerank(G, weight="weight")
print("dangling", [(k, repr(v)) for k, v in sorted(r.items())])
r = nx.pagerank(G, weight="weight", personalization=pers, dangling=pers)
print("dangling_pers", [(k, repr(v)) for k, v in sorted(r.items())])
print("networkx", nx.__version__)
