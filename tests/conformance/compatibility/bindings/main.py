from helpers import active as invoke
from helpers import standby


class _Abandoned:
    pass


def _bound(value, ignored):
    return value


invoke()
_bound(1, 2)
