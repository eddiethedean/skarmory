from fastapi import APIRouter, FastAPI

app = FastAPI()
live_router = APIRouter()
dormant_router = APIRouter()


@live_router.get("/active")
def active():
    return 1


@dormant_router.get("/inactive")
def dormant():
    return 2


app.include_router(live_router)
