#!/usr/bin/env python3
"""Claude consumes stdout privately; the credential stays in process memory."""
import os
print(os.environ["SPIKE_API_KEY"])
