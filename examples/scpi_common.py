"""Shared connection selection for Pico I/O Bridge SCPI examples."""

BOARD_HOSTS = (
    ("RP2040 CAN Bus Feather", "pico-io-can-feather.local"),
    ("Feather RP2040", "pico-io-feather.local"),
    ("KB2040", "pico-io-kb2040.local"),
)
SCPI_PORT = 5025


def select_visa_resource() -> str:
    """Prompt for a board and return its PyVISA socket resource."""
    print("Select Pico I/O Bridge board:")
    for number, (name, hostname) in enumerate(BOARD_HOSTS, start=1):
        print(f"  {number}. {name} ({hostname})")
    custom_number = len(BOARD_HOSTS) + 1
    print(f"  {custom_number}. Custom hostname or IP address")

    while True:
        choice = input(f"Board [1-{custom_number}]: ").strip()
        try:
            number = int(choice)
        except ValueError:
            print("Enter a number from the list.")
            continue

        if 1 <= number <= len(BOARD_HOSTS):
            hostname = BOARD_HOSTS[number - 1][1]
            break
        if number == custom_number:
            hostname = input("Hostname or IP address: ").strip()
            if hostname:
                break
            print("Hostname cannot be empty.")
            continue
        print("Enter a number from the list.")

    return f"TCPIP0::{hostname}::{SCPI_PORT}::SOCKET"
