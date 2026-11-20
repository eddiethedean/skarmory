from pkg import task as original
import pkg.task
from pkg import task as replacement
from pkg import fallback


original()
replacement.run()
fallback.run()
