#ifndef __TEE_DICE_H
#define __TEE_DICE_H

DiceResult DiceTeeHandoverMainFlow(size_t buffer_size, uint8_t *buffer,
								   size_t *actual_size);
DiceResult DiceTeeHandoverMainFlowChainOrigin(uint8_t *buffer, size_t buffer_size,
											  size_t *actual_size);
DiceResult DiceTeeHandoverMainFlowChain(const uint8_t *handover, size_t handover_size, uint8_t *buffer, size_t buffer_size,
										size_t *actual_size);
DiceResult DiceTeeHandoverMainFlowChainCodeHash(const uint8_t *handover, size_t handover_size,
												const uint8_t *codehash, size_t codehash_size,
												uint8_t *buffer, size_t buffer_size,
												size_t *actual_size);
void dice_init(void);

#endif