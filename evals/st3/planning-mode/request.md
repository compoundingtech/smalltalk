# Planning request

Create a mission for a small release inspection. The mission must have exactly two steps.

- `inspect` is a root step. Its goal is to inspect the release inputs.
- `verify` depends explicitly on completed `inspect`. Its goal is to verify the release decision.

Use the mission ID from your launch prompt. Set the mission state to `ready`. Include a clear mission goal and a useful title and goal for each step. The Markdown mission must describe the same dependency. Do not run or publish the mission. Do not change any file in the launch workspace.
