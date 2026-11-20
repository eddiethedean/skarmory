from .service import handle


def setup_default():
    pass


def unused(value=setup_default()):
    pass


def nested_target():
    pass


def launch():
    def handle():
        nested_target()

    handle()


def guarded():
    handle()


if __name__ == "__main__":
    guarded()
    launch()
