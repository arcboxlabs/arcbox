# 2026-10-01 — Why does a miss under `arcbox.local` take 10 s on macOS, and which answer ends it?

- Type: experiment
- Area: `virt/arcbox-net` (`DnsForwarder`), `app/arcbox-daemon` (`DnsService`)
- Outcome: unknown names under the local domain are answered NODATA instead of NXDOMAIN (same commit series as this entry)
- Probes: `tests/bench/dns-local-negative/`
- Host: Apple Silicon, macOS 26.4, `/etc/resolver/arcbox.local` installed by the production daemon (`nameserver 127.0.0.1`, `port 5553`, `timeout 5`), production daemon stopped so a probe could own the port

## Question

While moving the DNS bind ahead of the VM boot, the daemon's socket is now bound but silent during startup. Does that change what a client sees, and is there an answer the daemon could give that fails a lookup fast instead of letting it hang? The answer decides whether the startup window needs a "not ready" response and what the daemon should return for a name that is not registered.

## Hypotheses

1. A port with no listener fails a lookup at once (ICMP port unreachable), so binding early and staying silent makes lookups slower during startup.
2. An immediate SERVFAIL or REFUSED during the startup window ends the lookup faster than silence.
3. The daemon's NXDOMAIN for an unknown name ends a lookup at once.

## Method

`responder.py <mode>` on `127.0.0.1:5553` stands in for the daemon and answers every query one fixed way; `lookup.sh` times one `dscacheutil -q host -a name <fresh-name>.arcbox.local` (getaddrinfo → mDNSResponder → resolver file). One fresh name per lookup. Controls: a `.local` name with no resolver file (`foo.local`, pure mDNS) and a name under a normal unicast domain. `dig @127.0.0.1 -p 5553` checked the responder's answers parse.

## Results

| Listener on 5553 | Lookup | Result |
|---|---|---|
| nothing bound | 10.01 s | no address |
| bound, never answers | 10.01 s | no address |
| SERVFAIL at once | 10.03 s | no address |
| REFUSED at once | 10.01 s | no address |
| NXDOMAIN at once, no SOA | 10.01 s | no address |
| NXDOMAIN at once, SOA in authority | 10.02 s | no address |
| NODATA at once (NOERROR, no records) | 0.01 s | no address |
| A + AAAA records | 0.01 s | addresses |
| A record, NODATA for AAAA | 0.01 s | address |
| A record, no answer for AAAA | 5.01 s | address |
| control: `nothere.foo.local`, no resolver file | 10.01 s | no address |
| control: `nothere.example.com` | 0.01 s | answered by the system resolver |

The responder's log shows the client's schedule: AAAA at 0.45 s, retried at 1.5 s and 3.5 s, then A at 5.45 s, each type abandoned after 5 s. With NODATA for a type, the next type's query follows at once. `dig` against the unbound port timed out rather than reporting a refusal; `dig @127.0.0.1 -p 5553` received the SERVFAIL in 0.01 s. A name answered NODATA, then re-queried once the responder answered positively, resolved in 0.01 s: no negative cache was observed.

## Findings

1. Hypothesis 1 died. Nothing on the port behaves exactly like a silent listener for mDNSResponder and for `dig`: unconnected UDP sockets see no ICMP error on macOS. Binding early changes nothing a client can observe.
2. Hypothesis 2 died. SERVFAIL and REFUSED are ignored for a `.local` question; the lookup waits out the mDNS leg regardless.
3. Hypothesis 3 died, which matters more. NXDOMAIN is ignored too, with or without an SOA. Every miss under `arcbox.local` cost 10 s through getaddrinfo: 5 s per record type, the mDNS timeout, because RFC 6762 gives `.local` to mDNS and mDNS has no "name does not exist" answer. The `foo.local` control shows the same 10 s with no resolver file at all.
4. NODATA (NOERROR with an empty answer section) is the one negative form mDNSResponder accepts from the unicast server for a `.local` name. It ends the question immediately and was not cached. The same form already made AAAA-for-an-IPv4-name fast; extending it to unknown names makes a miss fail in milliseconds.
5. Only a positive answer or NODATA can shorten a lookup. There is no answer the daemon could give during startup that beats silence, so the bound-but-silent window needs no special handling.

## Decisions taken / open

- `DnsForwarder::try_resolve_locally_or_nodata` answers unknown names under the local domain NODATA; the rule and its reason live in `app/AGENTS.md`.
- The daemon that owns the resolver domain also registers its names with mDNSResponder (`arcbox-daemon/src/mdns/`), so they resolve on a Mac without `/etc/resolver` for clients allowed to resolve `.local` names; see the follow-ups below for the privilege that gates this and the interface the records go on.

## Follow-up, same day: Local Network privacy

Apple's TN3179 ("Understanding local network privacy") names the mechanism behind the `kDNSServiceErr_PolicyDenied` the first attempt at Bonjour registration hit, and it also explains a result above.

- Every Bonjour operation, registration included, and resolving any `.local` name both require the Local Network privilege on macOS 15+. Exempt: `launchd` daemons, root, and command-line tools started from Terminal.app or over SSH. A `launchd` agent is not exempt; it inherits the privilege of the app it belongs to, and macOS prompts for that app once. A short-lived process is denied without a prompt (FB16131937).
- The privilege is keyed to the responsible *app bundle*. The probes here ran under Homebrew's `python3`, which execs `Python.framework/…/Python.app`, so every probe was "Python" in System Settings › Privacy & Security › Local Network, whichever terminal launched it. That is why a rerun from Terminal.app was still refused (`PolicyDenied` in 1 ms, LocalOnly and Any alike) and why the terminal multiplexer the session ran in never appeared in the list. Once Python was allowed there, the same probe from the same shell registered (`err=0`) and the name resolved.
- The lookups above went through `dscacheutil`, which asks root's `opendirectoryd`, so they show what an *allowed* client sees. The same lookup from a client without the privilege fails at once, through every path: Python `getaddrinfo` for `lnp-<n>.arcbox.local` returned "nodename nor servname provided" in 0.00 s while the responder on 5553 was answering positively and `dscacheutil` returned the address in the same second. A client without Local Network access therefore cannot resolve `*.arcbox.local` at all; neither the resolver file nor Bonjour records reach it. That is a property of the `.local` suffix, not of either server.
- With Python allowed, `getaddrinfo` behaves like `dscacheutil` on the unicast answers: a miss with the responder answering NODATA fails in 0.00–0.01 s for `AF_UNSPEC`, `AF_INET` and `AF_INET6`; with NXDOMAIN or silence it takes 5.00 s (one mDNS timeout; `dscacheutil` pays two, 10 s). The NODATA answer holds for the client path too.

## Follow-up, same day: which interface the Bonjour records go on

Registered from the allowed Python, one A record for a fresh name, then `getaddrinfo` for the name with no resolver file involved (5553 had no listener):

| Registration | Registration reply | `AF_UNSPEC` lookup | `AF_INET6` lookup |
|---|---|---|---|
| `kDNSServiceInterfaceIndexLocalOnly`, A only | 0.00 s | 5.03 s, address | 5.00 s |
| LocalOnly, A + a client-registered NSEC (Unique or Shared) | 0.00 s | 5.01 s, address | 5.00 s |
| `kDNSServiceInterfaceIndexAny`, A only | 0.67 s (probing) | 0.01 s, address | 0.00 s |
| interface `lo0`, A only | 0.73 s (probing) | 0.01 s, address | 0.00 s |

mDNSResponder synthesizes the negative answer for a type the name lacks only for ordinary interface-bound records, not for the LocalOnly list, and ignores a client-registered NSEC. `lo0` has no multicast peers, so binding the records there keeps them on the host and still gets the fast negative; the mirror uses it. Probing costs about 750 ms before a record is live, which the unicast server covers.

The daemon itself, run from Terminal.app as a development-profile `--no-linux-vm` instance owning `w.dev.arcbox.local`, logged `mDNSResponder serves this host's *.w.dev.arcbox.local names`, and `host.w.dev.arcbox.local` resolved to 10.0.2.1 from `getaddrinfo`; the records were gone after SIGTERM.

## Decisions taken / open

- `DnsForwarder::try_resolve_locally_or_nodata` answers unknown names under the local domain NODATA; the rule and its reason live in `app/AGENTS.md`.
- The daemon that owns the resolver domain also registers its names with mDNSResponder (`arcbox-daemon/src/mdns/`), on `lo0`, so they resolve on a Mac without `/etc/resolver` for clients allowed to resolve `.local` names. Bonjour cannot make a miss fast (mDNS has no zone-wide denial), so the unicast server with its NODATA answer stays; denied clients need the user to allow them in System Settings › Privacy & Security › Local Network, the same step OrbStack users take for browsers on `orb.local` (its `/etc/resolver` is absent; the names are Bonjour records).
- Open: the explicit resolver file the test daemon had the helper write, `/etc/resolver/w.dev.arcbox.local`, never appeared in `scutil --dns` while `/etc/resolver/arcbox.local` did, so that domain had no unicast leg and its misses took 5 s. Whether macOS declines a resolver file for a subdomain of another resolver file's domain, or the helper's write is not picked up, is unanswered here.
