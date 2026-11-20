+++
title = "Netfilter"
paths = ["net/netfilter/", "net/ipv4/netfilter/", "net/ipv6/netfilter/"]
symbols = []
+++
# Netfilter: maintainer notes

Netfilter maintainer policy on where bugs in skbs handed to inet netfilter hooks get fixed.

- When an skb that violates an IP-stack precondition (for example skb_dst() returning NULL) can reach an inet netfilter hook only through br_netfilter, the bug belongs in net/bridge/ or net/bridge/br_netfilter*.c: br_netfilter must satisfy the IP-stack preconditions before it calls inet hooks. Do not ask for, or accept as a fix, NULL or state checks in xt matches/targets or nft expressions for such a path, and do not report sibling matches that lack the same check. Report the bridge-side defect instead. This does not apply when the same state can reach the hook without going through br_netfilter; show with git_grep or git_read_files that every such path goes through br_netfilter before using this rule.
