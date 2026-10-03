# `.local` negative-answer probes

What a macOS getaddrinfo client does with each kind of unicast DNS answer
for a name under a `.local` domain. They drove
`docs/experiments/2026-10-01-local-domain-negative-answers.md`.

`.local` is the mDNS domain (RFC 6762), so for `foo.arcbox.local`
mDNSResponder multicasts the question and, because of
`/etc/resolver/arcbox.local`, also sends it to the daemon's listener on
`127.0.0.1:5553`. These probes replace that listener with one that answers
a fixed way, and time the lookup. Run them with the production daemon
stopped (the port must be free); nothing here touches the resolver file.

| Script | What it does |
|---|---|
| `responder.py <mode> [port]` | Binds `127.0.0.1:<port>` (default 5553) and answers every query per `mode`: `silent`, `nxdomain`, `nxdomain-soa`, `servfail`, `refused`, `nodata`, `positive`, `a-only`, `a-silent`. Prints each query it receives with its offset in seconds, which shows the client's retry schedule. |
| `lookup.sh <label> [domain]` | One `dscacheutil -q host -a name <label>-<random>.<domain>` lookup, timed; prints the addresses returned. The random suffix defeats mDNSResponder's cache. Pass a domain with no resolver file (`foo.local`) to measure the pure mDNS path. |

Typical run:

```sh
python3 responder.py nxdomain & sleep 0.5; ./lookup.sh nxdomain; kill %1
python3 responder.py nodata   & sleep 0.5; ./lookup.sh nodata;   kill %1
```

Read a 10 s result as two 5 s mDNS timeouts (AAAA, then A); a 0.01 s result
means the unicast answer ended the question.
