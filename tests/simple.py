from openlimits_python import ExchangeClient
from dotenv import load_dotenv
import os

load_dotenv()

nash_creds = {
    "nash": {
        "credentials": {
            "nash_credentials": {
                "secret": os.getenv("NASH_API_SECRET"),
                "session": os.getenv("NASH_API_KEY")
            },
        },
        "client_id": 0,
        "environment": "sandbox",
        "affiliate_code": None,
        "timeout": 10000
    }
}

client = ExchangeClient(nash_creds)
balance = client.get_account_balances(None);
print(balance)