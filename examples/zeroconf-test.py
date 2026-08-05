import socket
import time
from zeroconf import ServiceBrowser, ServiceInfo, ServiceListener, Zeroconf


class HTTPServiceListener(ServiceListener):

    def add_service(self, zc: Zeroconf, type_: str, name: str) -> None:
        """Anropas så fort en ny enhet upptäcks på nätverket."""
        print(f"\n[+] Hittade enhet: {name}")

        # Hämta detaljerad information om den specifika enheten
        info: ServiceInfo = zc.get_service_info(type_, name)

        if info:
            # Översätt IP-adressen från bytes till läsbar text
            addresses = [
                socket.inet_ntoa(addr)
                for addr in info.addresses
                if len(addr) == 4
            ]
            print(f"    IP-adress(er): {', '.join(addresses)}")
            print(f"    Port:         {info.port}")
            print(f"    Servernamn:   {info.server}")

            # Om enheten skickar med några TXT-records (metadata) skrivs de ut här
            if info.properties:
                print("    Egenskaper (TXT):")
                for key, val in info.properties.items():
                    # TXT-värden kan vara bytes, så vi avkodar dem till strängar
                    decoded_val = (
                        val.decode("utf-8") if isinstance(val, bytes) else val
                    )
                    print(f"      {key.decode('utf-8')}: {decoded_val}")
        else:
            print("    Kunde inte hämta ytterligare detaljer.")

    def update_service(self, zc: Zeroconf, type_: str, name: str) -> None:
        """Anropas om en befintlig enhet ändrar information."""
        pass

    def remove_service(self, zc: Zeroconf, type_: str, name: str) -> None:
        """Anropas om en enhet försvinner från nätverket."""
        print(f"\n[-] Enhet borttagen: {name}")


def main():
    # Initiera Zeroconf-klienten
    zeroconf = Zeroconf()

    # Den tjänstetyp vi vill leta efter (HTTP-servrar över TCP)
    service_type = "_http._tcp.local."

    listener = HTTPServiceListener()
    print(f"Söker efter enheter som annonserar {service_type}...")
    print("Sökningen pågår i 5 sekunder (tryck Ctrl+C för att avbryta)...")

    # Starta bakgrundsavsökningen
    browser = ServiceBrowser(zeroconf, service_type, listener)

    try:
        # Låt sökningen köra i 5 sekunder så nätverket hinner svara
        time.sleep(5)
    except KeyboardInterrupt:
        pass
    finally:
        # Stäng ner snyggt när vi är klara
        zeroconf.close()


if __name__ == "__main__":
    main()
