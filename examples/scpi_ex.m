% Load the instrument-control package.
pkg load instrument-control;

% Use the Pico's hostname on the virtual CDC-NCM network.
pico_ip = "pico-io-can-feather.local";
pico_port = 5025;

% Open a TCP connection and use the SCPI line ending as the terminator.
pico = tcpclient(pico_ip, pico_port, "Timeout", 2);
configureTerminator(pico, "lf");

% writeread waits for a complete LF-terminated response.
identity = writeread(pico, "*IDN?");
fprintf("Connected to: %s\n", char(identity));

% Read the voltage measured on ADC channel 1.
response = writeread(pico, "MEASure:VOLTage:DC? 1");
adc_voltage = str2double(char(response));

fprintf("ADC channel 1 voltage: %.3f V\n", adc_voltage);

% Close the connection.
clear pico;
