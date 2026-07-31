function scpi_usb_host_p8055(host, sample_count, pulse_output, pulse_seconds, port)
  % Read and optionally exercise a P8055 through Pico I/O Bridge SCPI.
  %
  % The default run is read-only:
  %   scpi_usb_host_p8055
  %
  % Explicitly toggle digital output 1 for half a second:
  %   scpi_usb_host_p8055("pico-io-usb-host.local", 5, 1, 0.5)

  if nargin < 1, host = "pico-io-usb-host.local"; endif
  if nargin < 2, sample_count = 5; endif
  if nargin < 3, pulse_output = 0; endif
  if nargin < 4, pulse_seconds = 0.5; endif
  if nargin < 5, port = 5025; endif

  if sample_count < 1 || sample_count != fix(sample_count)
    error("sample_count must be a positive integer");
  endif
  if pulse_output < 0 || pulse_output > 8 || pulse_output != fix(pulse_output)
    error("pulse_output must be zero (disabled) or an integer from 1 to 8");
  endif
  if pulse_seconds < 0
    error("pulse_seconds must be non-negative");
  endif

  pkg load instrument-control;
  pico = tcpclient(host, port, "Timeout", 6);
  configureTerminator(pico, "lf");

  unwind_protect
    writeline(pico, "*CLS");
    identity = scpi_query(pico, "*IDN?");
    status = scpi_query(pico, "SYST:USB:HOST:STAT?");
    require_p8055(status);
    outputs = parse_uint_csv(
      scpi_query(pico, "SYST:USB:HOST:P8055:OUTP?"), 3, "P8055 output");

    fprintf("Connected to: %s\n", identity);
    fprintf("Host status: %s\n", status);
    fprintf("Outputs: digital=0x%02X, analog1=%u, analog2=%u\n",
            outputs(1), outputs(2), outputs(3));

    if pulse_output > 0
      pulse = outputs;
      pulse(1) = bitxor(pulse(1), bitshift(1, pulse_output - 1));
      output_may_have_changed = false;
      unwind_protect
        output_may_have_changed = true;
        set_and_verify_output(pico, pulse);
        fprintf("Pulsing digital output %u for %g s\n",
                pulse_output, pulse_seconds);
        pause(pulse_seconds);
      unwind_protect_cleanup
        if output_may_have_changed
          try
            set_and_verify_output(pico, outputs);
            fprintf("Original output state restored and verified\n");
          catch cleanup_error
            warning(["Output restoration could not be verified; replug the " ...
                     "P8055 before further output commands: %s"],
                    cleanup_error.message);
            error(["Output state is uncertain; physically replug the P8055 " ...
                   "before issuing another output command"]);
          end_try_catch
        endif
      end_unwind_protect
    endif

    fprintf("digital  analog1  analog2  counter1  counter2\n");
    for sample = 1:sample_count
      values = parse_uint_csv(
        scpi_query(pico, "SYST:USB:HOST:P8055:INP?"), 5, "P8055 input");
      fprintf("0b%s%9u%9u%10u%10u\n",
              dec2bin(bitand(values(1), 31), 5),
              values(2), values(3), values(4), values(5));
      if sample < sample_count
        pause(0.5);
      endif
    endfor
  unwind_protect_cleanup
    clear pico;
  end_unwind_protect
endfunction

function response = scpi_query(pico, command)
  response = strtrim(char(writeread(pico, command)));
endfunction

function values = parse_uint_csv(response, expected_count, name)
  fields = strsplit(response, ",");
  values = str2double(fields);
  if numel(values) != expected_count || any(!isfinite(values)) ...
      || any(values < 0) || any(values != fix(values))
    error("Invalid %s response: %s", name, response);
  endif
endfunction

function require_p8055(status)
  fields = strsplit(status, ",");
  if numel(fields) < 9
    error("Unexpected USB host status: %s", status);
  endif
  vendor_id = str2double(fields{4});
  product_id = str2double(fields{5});
  max_transfer = str2double(fields{9});
  if !strcmp(fields{1}, "P8055_READY") || !strcmp(fields{2}, "LOW") ...
      || vendor_id != hex2dec("10CF") ...
      || product_id < hex2dec("5500") || product_id > hex2dec("5503") ...
      || max_transfer != 8
    error("P8055 is not ready: %s", status);
  endif
endfunction

function set_and_verify_output(pico, expected)
  command = sprintf("SYST:USB:HOST:P8055:OUTP %u,%u,%u", expected);
  writeline(pico, command);
  scpi_error = scpi_query(pico, "SYST:ERR?");
  if !(strncmp(scpi_error, "0,\"", 3) || strncmp(scpi_error, "+0,\"", 4))
    error("P8055 output command failed: %s", scpi_error);
  endif
  confirmed = parse_uint_csv(
    scpi_query(pico, "SYST:USB:HOST:P8055:OUTP?"), 3, "P8055 output");
  if any(confirmed != expected)
    error("Output verification failed");
  endif
endfunction
