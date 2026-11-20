def handle():
    live_left()


def live_left():
    live_right()


def live_right():
    live_left()


def dead_left():
    dead_right()


def dead_right():
    dead_left()
