from helper import work as run
import other as secondary

def launch():
    run()
    secondary.entry()

def dead_left():
    dead_right()

def dead_right():
    dead_left()

launch()
