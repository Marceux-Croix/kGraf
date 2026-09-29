# Decision 0051: kGraf stays single-user; hosted MCP servers are allowed

**Status:** Accepted. Amends decision 0005.
**Related:** decision 0005

This record exists so multi-tenant machinery found in reference client implementations is
not imported by accident, and so a hosted Model Context Protocol (MCP) server is not refused
on an overly strict reading of the network-egress rule below.

## Decision

1. **kGraf is single-user.** It has no tenants, no per-user access-control lists, no sharing
   permissions, no replicas, and no cross-process coordination between separate users. Any
   reference code that exists to serve those concerns is out of scope: per-user credential
   stores, shared catalogs meant for many concurrent users, versioning across rolling
   deployments, and tenant isolation are all not needed here.

2. **Hosted MCP servers are allowed.** kGraf's MCP client speaks stdio for local servers and
   Streamable HTTP for remote ones. It handles authentication for a remote server that
   requires it, on behalf of the single local user, with the resulting token held on that
   user's own machine.

3. **The network-egress rule is amended.** Permitted network traffic now covers two
   destinations: the model providers the user configured, and the MCP servers the user
   configured. Both are chosen by the user and use the user's own credentials. kGraf still
   operates no service of its own, sends no telemetry, and keeps no account.

## Context

kGraf's MCP client was originally scoped to stdio only, which in effect confined it to local
servers. Hosted MCP servers needed to be allowed as the product's use grew to include remote
tools.

The intent of the network-egress rule was never "no byte leaves the machine": a model
provider already receives code as part of normal operation. The intent was that kGraf itself
never operates a network destination, and that no traffic happens that the user did not
configure. A hosted MCP server the user added themselves falls inside that intent, so the rule
is amended rather than replaced.

## Consequences

A hosted MCP server receives whatever tool arguments a worker sends it, which can include
code. That is the same exposure a model provider already has, and the configuration screen
for adding a remote server states this.

Every remote MCP call is attributed to the agent and task that made it, the same way provider
calls are. It costs no tokens directly, but it is still outbound network traffic, and it must
be visible to the user.

Tool-list changes on a remote server are applied only at the next dispatch, never mid-run,
because tool definitions sit in the shared prefix described in decision 0043.

**Reactivation trigger.** Any request for a second user, a shared deployment, or a
kGraf-operated relay server. Each would need its own decision, and the prohibition on a hosted
mode for kGraf itself still stands against all three.
