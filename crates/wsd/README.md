# craft-wsd

The protocol-agnostic core for Web Services for Devices (WSD) clients: the pieces every device
service shares, so a print client, a scan client or anything else only adds its own operations,
data model and fault codes. No `unsafe`, no panics on hostile replies, no dependency on any app
or UI framework. Requires Rust 1.95.

```text
xml        bounded parsing (size, depth, node count, no DTD), QName, qualified lookup, escaping
envelope   SOAP 1.2 + WS-Addressing request envelopes, checked reply reading, UUIDs
fault      SOAP faults as values: role, sub-code (namespace + local name), reason, detail
addressing endpoint references and reference parameters
mtom       MTOM/XOP packaging, base64, a bounded multipart reader
policy     DeviceUrl (numeric local-network addresses only), NetworkLimits
transport  the Transport trait over one HTTP POST; Request and Response
http       HttpTransport on ureq, no TLS (not built for wasm32, which has no native HTTP client)
soap       SoapClient<T: Transport> and Call: one exchange, ids, limits, deadline, cancel, faults
service    ServiceKind (print, scan) and ServiceDescriptor: types, namespace, action table
metadata   DeviceMetadata, WS-Transfer Get / GetMetadata, endpoint choice by service type
eventing   WS-Eventing client: subscribe, renew, status, unsubscribe; notifications; sink trait
discovery  WS-Discovery messages, matching, DiscoverySocket trait, discover() (no sockets inside)
cancel     CancelToken
testing    loopback HTTP server and an in-memory FakeDevice (cargo feature `testing`)
error      Error: policy, transport, connection_closed, http, too_large, xml, protocol, fault,
           invalid, cancelled, timeout, not_found
```

## Building a service client on it

```rust,ignore
// Describe the exchange; the descriptor knows the namespace and the action table.
let scan = ServiceKind::Scan.descriptor();
let call = scan.call("GetScannerElements", "<wscn:GetScannerElementsRequest/>");
// One client per endpoint: limits, ids, cancel and deadline are handled for every call.
let client = SoapClient::new(transport, scanner_url).with_budget(Duration::from_secs(60));
// The reply's first Body element goes to a reader that returns the service crate's own error
// type (it needs From<craft_wsd::Error>); faults, HTTP errors and mismatches are mapped for you.
let elements = client.call(&call, read_scanner_elements)?;
```

Device metadata and events work the same way:

```rust,ignore
let meta = metadata::transfer_get(&SoapClient::new(&transport, device_url))?;
let scanner_url = meta.endpoint_for(ServiceKind::Scan)?;      // only policy-admitted addresses
let sub = eventing::subscribe(&client, &SubscribeRequest::new(EndpointRef::new(my_sink_url)))?;
eventing::dispatch(&posted_bytes, &my_sink)?;                 // from the application's listener
```

## Where it may connect: the address policy

The policy limits where the crate talks, whatever transport is used.

* A `DeviceUrl` is `http://` plus a **numeric** IPv4/IPv6 address that is loopback, link-local,
  RFC 1918 private or IPv6 unique-local, with an optional port and a path. Host names (so no
  DNS), public addresses, multicast, unspecified, carrier-grade NAT, IPv4-mapped IPv6, user
  information, queries, fragments and `https` are refused.
* A `Transport` only ever receives a `DeviceUrl`, and must use no proxy, follow no redirects,
  time out, and cap the reply (`NetworkLimits`).
* Addresses taken from a device (metadata endpoints, subscription managers, discovery `XAddrs`)
  and addresses handed to a device (`NotifyTo`, `EndTo`) go through the same policy. Discovery
  datagrams are accepted only from admitted source addresses.
* XML replies are capped at 4 MiB, 100,000 nodes and 40 levels; DTDs are refused.

## Network I/O

The message layer, the policy, `SoapClient` (generic over the `Transport` trait) and the
discovery driver (generic over the `DiscoverySocket` trait) contain no sockets.

* `http::HttpTransport` is a blocking `ureq` 3 client built with its default
  features off (no TLS, no cookies, no compression): proxies are ignored even if the environment
  sets one, redirects are refused, there is a timeout, the reply is read through a hard size cap,
  and a connection the device closes without replying fails at once as `Error::ConnectionClosed`
  (retryable). It can only be pointed at an address a `DeviceUrl` admits, so it connects to
  local-network devices and nothing else. It adds `ureq`, `ureq-proto`, `http`, `httparse`,
  `bytes`, `base64`, `log`, `itoa`, `percent-encoding` and `utf8-zero` to the dependency graph.
  It is not compiled for `wasm32`, where the crate stays message-level only.
* There is no multicast code and no listener. `discovery::discover` takes a `DiscoverySocket`;
  the only implementation here is in memory. Event delivery is left to the application, which
  posts received bodies into `eventing::dispatch`.
* Deciding when to contact a device (for example asking the person first) belongs to the
  application. A service client can add its own consent marker type on top of this crate.

## Features

| Feature   | Adds |
|-----------|------|
| `testing` | `testing::Server` (loopback HTTP), `testing::FakeDevice`, `discovery::fake` |

## Tests

Unit tests beside each module (hostile XML, duplicate and oversized elements, wrong namespaces,
mismatched replies, multipart edge cases, URL policy, sequence and matching rules);
`tests/two_services.rs` drives the same code against a print-shaped and a scan-shaped in-memory
device; `tests/http_transport.rs` runs the real transport against loopback
servers (refused redirects, capped replies, timeouts, early close); `tests/live_loopback.rs`
checks metadata and eventing against a loopback fake device you start yourself,
and is skipped unless `CRAFT_WSD_LIVE_URL` is set. No test contacts anything but loopback.

```text
cargo test -p craft-wsd
```
