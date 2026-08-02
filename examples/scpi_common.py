"""Shared connection selection for Pico I/O Bridge SCPI examples."""

BOARD_HOST_PREFIXES = (
    ("RP2040 CAN Bus Feather", "pico-io-can-feather"),
    ("Feather RP2040", "pico-io-feather"),
    ("Feather RP2040 USB Host", "pico-io-usb-host"),
    ("KB2040", "pico-io-kb2040"),
)
SCPI_PORT = 5025


def select_visa_resource() -> str:
    """Prompt for a board and return its PyVISA socket resource."""
    print("Select Pico I/O Bridge board:")
    for number, (name, prefix) in enumerate(BOARD_HOST_PREFIXES, start=1):
        print(f"  {number}. {name} ({prefix}-<uid6>.local)")
    custom_number = len(BOARD_HOST_PREFIXES) + 1
    print(f"  {custom_number}. Custom hostname or IP address")

    while True:
        choice = input(f"Board [1-{custom_number}]: ").strip()
        try:
            number = int(choice)
        except ValueError:
            print("Enter a number from the list.")
            continue

        if 1 <= number <= len(BOARD_HOST_PREFIXES):
            prefix = BOARD_HOST_PREFIXES[number - 1][1]
            suffix = input("UID suffix (last six USB-serial hex digits): ").strip().lower()
            if len(suffix) != 6 or any(character not in "0123456789abcdef" for character in suffix):
                print("UID suffix must be exactly six hexadecimal digits.")
                continue
            hostname = f"{prefix}-{suffix}.local"
            break
        if number == custom_number:
            hostname = input("Hostname or IP address: ").strip()
            if hostname:
                break
            print("Hostname cannot be empty.")
            continue
        print("Enter a number from the list.")

    return f"TCPIP0::{hostname}::{SCPI_PORT}::SOCKET"
