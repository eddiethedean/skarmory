def main_branch():
    pass


def other_branch():
    pass


def reached_by_callback():
    pass


def factory(__name__):
    if __name__ == "__main__":
        main_branch()
    else:
        other_branch()

    def callback():
        reached_by_callback()

    return callback


def private():
    pass


metadata = 1
__all__ = ["factory", "metadata"]
if False:
    __all__ = ["private"]
