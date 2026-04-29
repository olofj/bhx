// SPDX-License-Identifier: GPL-2.0+
// SPDX-FileCopyrightText: © 2026 Olof Johansson
//
// DM serial driver wrapping the SBI v2.0 DBCN extension. Lets U-Boot
// use OpenSBI's debug console (the existing pre-relocation
// CONFIG_DEBUG_SBI_CONSOLE path) as the post-relocation interactive
// console too. Without this, U-Boot bails after relocation with
// "No serial driver found" because qemu-riscv64_smode_defconfig's
// NS16550 + SIFIVE_SERIAL drivers can't bind to anything on a
// platform whose console is the SBI debug console (no MMIO UART).
//
// Bind: a /chosen/sbi-console node with compatible
// "riscv,sbi-debug-console". The daemon's modify_dtb adds this.
//
// I/O: write goes through sbi_dbcn_write_byte (already in U-Boot's
// arch/riscv/lib/sbi.c). Read calls SBI_EXT_DBCN_CONSOLE_READ with a
// 1-byte buffer; if no data is available the call returns
// value=0, and we surface that as -EAGAIN so U-Boot's serial-uclass
// poll loop spins until a byte arrives.

#include <dm.h>
#include <errno.h>
#include <serial.h>
#include <asm/sbi.h>

static int sbi_dm_serial_setbrg(struct udevice *dev, int baudrate)
{
	/* No baud rate to set — the SBI console is a virtual ring. */
	return 0;
}

static int sbi_dm_serial_putc(struct udevice *dev, const char ch)
{
	int err = sbi_dbcn_write_byte((unsigned char)ch);

	/* SBI_SUCCESS = 0; non-zero error implies the console is full
	 * (SBI_ERR_DENIED on EAGAIN-equivalent, per spec).  Return
	 * -EAGAIN so the serial-uclass write loop retries.
	 */
	if (err)
		return -EAGAIN;
	return 0;
}

/* SBI DBCN has no "data available" query — the only way to test for
 * input is to actually try reading. We cache one byte here so
 * `pending(true)` can return a truthful answer: it speculatively
 * reads, stashes the byte, and returns 1; the next getc serves from
 * the cache. If pending didn't buffer, returning 1 unconditionally
 * would aborts autoboot every tick (autoboot calls tstc → pending
 * thinks "key pressed", calls getc → getc returns -EAGAIN → infinite
 * spin since no real key was pressed).
 */
static int rx_cache_valid __section(".data");
static unsigned char rx_cache __section(".data");

static int sbi_dm_try_read(unsigned char *out)
{
	struct sbiret ret;

	ret = sbi_ecall(SBI_EXT_DBCN, SBI_EXT_DBCN_CONSOLE_READ,
			1, (unsigned long)out, 0, 0, 0, 0);
	if (ret.error || ret.value == 0)
		return 0;
	return 1;
}

static int sbi_dm_serial_getc(struct udevice *dev)
{
	unsigned char ch;

	if (rx_cache_valid) {
		rx_cache_valid = 0;
		return rx_cache;
	}
	if (sbi_dm_try_read(&ch))
		return ch;
	return -EAGAIN;
}

static int sbi_dm_serial_pending(struct udevice *dev, bool input)
{
	if (!input)
		return 0;
	if (rx_cache_valid)
		return 1;
	if (sbi_dm_try_read(&rx_cache)) {
		rx_cache_valid = 1;
		return 1;
	}
	return 0;
}

static const struct udevice_id sbi_dm_serial_ids[] = {
	{ .compatible = "riscv,sbi-debug-console" },
	{ }
};

static const struct dm_serial_ops sbi_dm_serial_ops = {
	.putc    = sbi_dm_serial_putc,
	.getc    = sbi_dm_serial_getc,
	.pending = sbi_dm_serial_pending,
	.setbrg  = sbi_dm_serial_setbrg,
};

U_BOOT_DRIVER(serial_sbi_dm) = {
	.name     = "serial_sbi_dm",
	.id       = UCLASS_SERIAL,
	.of_match = sbi_dm_serial_ids,
	.ops      = &sbi_dm_serial_ops,
};
