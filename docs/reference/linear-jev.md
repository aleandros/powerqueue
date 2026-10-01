# Linear and Jev API facts

## Linear GraphQL

* Endpoint `https://api.linear.app/graphql`, `POST` JSON `{ "query": "...", "variables": {...} }`.
* Auth header for personal API keys: `Authorization: <key>` — **no** `Bearer` prefix.
* Errors come back as HTTP 200 with `errors: [...]`, or HTTP 400/401/429/5xx. Respect `Retry-After` on 429.
* Priority: `0` none, `1` urgent, `2` high, `3` normal, `4` low.
* Workflow state `type`: `triage | backlog | unstarted | started | completed | canceled`.

Queries used:

```graphql
query { viewer { id name email } }
query { teams { nodes { id key name } } }
query($key: String!) { workflowStates(filter: { team: { key: { eq: $key } } }) { nodes { id name type team { key } } } }
query($filter: IssueFilter, $first: Int, $after: String) {
  issues(filter: $filter, first: $first, after: $after, orderBy: updatedAt) {
    nodes { id identifier title description url priority estimate
            labels { nodes { name } } state { name type } team { key }
            project { name } assignee { id } createdAt updatedAt }
    pageInfo { hasNextPage endCursor }
  }
}
query($id: String!) { issue(id: $id) { ...same fields } }
mutation($id: String!, $stateId: String!) { issueUpdate(id: $id, input: { stateId: $stateId }) { success } }
mutation($issueId: String!, $body: String!) { commentCreate(input: { issueId: $issueId, body: $body }) { success } }
```

Filter example: `{ team: { key: { in: ["ENG"] } }, state: { name: { in: ["Todo"] } }, assignee: { id: { eq: "<viewer id>" } } }`.
Label filtering is done client-side (labels are a connection).

## Jev (TypeSafe "System One")

* `POST https://api.typesafe.ai/v1/systemone`, header `Authorization: Bearer <key>`.
* Request:

```json
{ "model": "jev-latest",
  "state": { "title": "...", "description": "...", "labels": ["bug"], "priority": "high" },
  "questions": { "priority": { "type": "score",
                                "instructions": "How important is it to ship `title` this week?",
                                "criteria": ["can wait", "nice to have", "important", "blocking"] } } }
```

* Response:

```json
{ "model": "jev-1.13.0",
  "answers": { "priority": { "type": "score", "score": 1.43,
                             "legend": { "0": "can wait", "1": "nice to have", "2": "important", "3": "blocking" },
                             "probabilities": { "0": 0.0, "1": 0.57, "2": 0.43, "3": 0.0 }, "confidence": 0.35 } },
  "usage": { "input_tokens": 210, "output_tokens": 31 } }
```

* `score` = probability-weighted mean of level indices. Pricing is per input token (cheap); responses are ~100 ms.
