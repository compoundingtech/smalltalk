# Free mode eval

This model-free eval proves [free mode](../../../docs/st3/kdl-lifecycle.md#free-mode): within a
fleet, an agent with no grant publishes, starts, and revises missions as itself.

A run-local planner, whose declaration carries no authority block, publishes a mission through a
claimed producing step, publishes it again directly, and starts it. A seat inside the produced run
revises that run and then starts a second run of the same mission. Each run names the agent that
started it as its requester.
