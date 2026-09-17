export type FinancialStatus = "PENDING" | "PAID";

export interface MailingAddress {
  address1?: string;
  address2?: string;
  city?: string;
  province?: string;
  country?: string;
  zip?: string;
}

export interface ManualCustomer {
  email?: string;
  firstName?: string;
  lastName?: string;
  phone?: string;
  shippingAddress?: MailingAddress;
}

export type CustomerMode =
  | { mode: "none" }
  | { mode: "existing"; customerId: string }
  | { mode: "manual"; customer: ManualCustomer };

export interface OrderTemplate {
  variantId: string;
  quantity: number;
  customer: CustomerMode;
  financialStatus: FinancialStatus;
}

export interface AppErrorDto {
  code: string;
  message: string;
  retryable: boolean;
}
