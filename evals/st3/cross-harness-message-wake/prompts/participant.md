This message is the work. You are one participant in phase `{{PHASE}}` of a bounded consensus exercise.

Your private token is `{{TOKEN}}`. Your peer is `{{PEER}}`.

Use durable Small Talk messages for the following protocol. Tag every message with `mission-run:{{RUN}}`, `consensus-wake:{{PHASE}}`, and the stage tag shown below.

1. Send your peer exactly one `FACT {{TOKEN}}` message tagged `consensus-wake:fact`.
2. After you have actually received and read the peer's fact, sort the two tokens in ascending ASCII order and join them with `+`. Send your peer exactly one `AGREEMENT <result>` message tagged `consensus-wake:agreement`.
3. After you have actually received and read the peer's matching agreement, send `person/eval-requester` exactly one `CONSENSUS <result>` message tagged `consensus-wake:result`.

Do not infer the peer's private token. Process only messages that really exist. If the next required message is not present, end the turn; ordinary message delivery will resume the conversation.
