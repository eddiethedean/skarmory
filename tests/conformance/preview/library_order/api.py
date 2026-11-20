from switches import initial, mode


def first():
    pass


def second():
    pass


if mode is initial:
    __all__ = ["first"]
else:
    __all__ = ["second"]
